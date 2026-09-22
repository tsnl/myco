"""Exercise the embedded UI in Chromium with real tools and a local scripted model."""

import argparse
import http.server
import json
import os
from pathlib import Path
import re
import selectors
import shlex
import signal
import subprocess
import tempfile
import threading
import time
import unittest
from urllib.parse import urlsplit
import uuid

from playwright.sync_api import expect, sync_playwright


def reply(text):
    return [{"type": "response.output_text.delta", "output_index": 0,
             "content_index": 0, "delta": text}]


def tool(arguments, name="bash", index=0):
    return [
        {"type": "response.output_item.added", "output_index": index,
         "item": {"type": "function_call", "name": name,
                  "call_id": str(uuid.uuid4()), "arguments": ""}},
        {"type": "response.function_call_arguments.done", "output_index": index,
         "arguments": json.dumps(arguments)},
    ]


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        fixture = self.server.fixture
        fixture.requests.append(body)
        prompts = []
        for item in body["input"]:
            if item.get("role") != "user":
                continue
            content = item["content"]
            if not isinstance(content, str):
                content = " ".join(part.get("text", "") for part in content)
            match = re.search(r"(Alpha|Beta) (shell|read marker|wait|stream|markdown|rename|parallel)\b", content)
            if match:
                prompts.append(match.group(0))
        prompt = prompts[-1]
        count = fixture.turns.get(prompt, 0)
        fixture.turns[prompt] = count + 1
        name = "Alpha" if "Alpha" in prompt else "Beta"
        root = fixture.home
        if count == 1 and "rename" in prompt:
            release = shlex.quote(str(root / (name + "-rename-release")))
            events = tool({"command": f"while [ ! -f {release} ]; do sleep 0.05; done", "timeout_ms": 60000})
        elif count:
            events = reply(f"{name} finished.")
        elif "rename" in prompt:
            events = tool({"action": "set_title", "title": f"Renamed {name}"}, "session_meta")
        elif "parallel" in prompt:
            events = []
            for index in range(2):
                release = shlex.quote(str(root / f"{name}-release-{index}"))
                events += tool({"command": f"while [ ! -f {release} ]; do sleep 0.05; done", "timeout_ms": 60000}, index=index)
        elif "shell" in prompt:
            events = tool({"action": "start", "session_id": "kept",
                           "command": f"MYCO_TEST_MARKER={name} bash --noprofile --norc",
                           "timeout_ms": 150, "idle_ms": 30})
        elif "read marker" in prompt:
            events = tool({"action": "write", "session_id": "kept",
                           "stdin": f'printf "$MYCO_TEST_MARKER" > {shlex.quote(str(root / (name + "-marker")))}; echo ready\n',
                           "timeout_ms": 150, "idle_ms": 30})
        elif "wait" in prompt:
            release = shlex.quote(str(root / (name + "-release")))
            done = shlex.quote(str(root / (name + "-done")))
            events = tool({"command": f"while [ ! -f {release} ]; do sleep 0.05; done\nprintf done > {done}", "host": "local",
                           "timeout_ms": 60000})
        elif "stream" in prompt:
            events = [event for i in range(30) for event in reply(f"Chunk {i}.\n\n")]
        else:
            events = reply("# Heading\n\n**Bold** and _emphasis_.\n\n"
                           "| Name | Value |\n| --- | --- |\n| Tool | Ready |\n\n"
                           f"![Local image]({root / 'pixel.png'})\n\n"
                           + "\n\n".join(f"Paragraph {i}: session output." for i in range(24)))
        events.append({"type": "response.completed", "response": {
            "status": "completed", "usage": {"input_tokens": 100, "output_tokens": 20}}})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            for event in events:
                self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
                self.wfile.flush()
                if "stream" in prompt:
                    time.sleep(0.01)
            self.wfile.write(b"data: [DONE]\n\n")
        except (BrokenPipeError, ConnectionResetError):
            pass


class BrowserTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.playwright = sync_playwright().start()
        cls.addClassCleanup(cls.playwright.stop)
        cls.browser = cls.playwright.chromium.launch(executable_path=OPTIONS.browser)
        cls.addClassCleanup(cls.browser.close)
        expect.set_options(timeout=10000)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="myco-browser-test-")
        self.addCleanup(temporary.cleanup)
        self.home = Path(temporary.name)
        # A transient startup notice must not make later snapshots replace the conversation.
        workspace = self.home / "profiles/default/workspace"
        workspace.mkdir(parents=True)
        (workspace / "prelude").touch()
        self.artifacts = OPTIONS.artifacts / self._testMethodName
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.requests = []
        self.turns = {}
        self.processes = []
        self.errors = []
        provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        provider.fixture = self
        self.addCleanup(provider.server_close)
        self.addCleanup(provider.shutdown)
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        (self.home / "config.toml").write_text('model = "first"\n' + "".join(f'''
[models.{name}]
protocol = "openai-responses"
base_url = "http://127.0.0.1:{provider.server_port}"
auth = {{ source = "none" }}
context_window = 100000
''' for name in ["first", "second"]))
        import base64
        (self.home / "pixel.png").write_bytes(base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6cGkAAAAASUVORK5CYII="))
        self.process, launch = self.launch()
        parsed = urlsplit(launch)
        self.origin = f"{parsed.scheme}://{parsed.netloc}"
        self.context = self.browser.new_context(viewport={"width": 1200, "height": 850})
        self.addCleanup(self.context.close)
        self.context.on("page", lambda page: page.on("pageerror", lambda error: self.errors.append(str(error))))
        self.context.tracing.start(screenshots=True, snapshots=True)
        self.page = self.context.new_page()
        self.page.goto(launch)

    def tearDown(self):
        for index, page in enumerate(self.context.pages):
            page.screenshot(path=str(self.artifacts / f"page-{index}.png"))
        self.context.tracing.stop(path=str(self.artifacts / "trace.zip"))
        (self.artifacts / "requests.json").write_text(json.dumps(self.requests, indent=2))
        self.assertEqual(self.errors, [])

    def launch(self, port=0, resume=None):
        log = (self.artifacts / f"server-{len(self.processes)}.log").open("w")
        self.addCleanup(log.close)
        args = [str(OPTIONS.binary), "--web", str(port), "--config", str(self.home / "config.toml")]
        if resume:
            args += ["--resume", resume]
        process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=log, text=True,
                                   cwd=self.home, env=dict(os.environ, MYCO_HOME=str(self.home), MYCO_PROFILE="default"))
        self.processes.append(process)
        self.addCleanup(process.stdout.close)
        self.addCleanup(self.stop, process)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            self.assertTrue(selector.select(15), "Server did not print its launch URL")
        launch = process.stdout.readline().strip().removeprefix("Browser UI: ")
        self.assertTrue(launch.startswith("http://"), f"Server failed to launch; see {log.name}")
        return process, launch

    def stop(self, process):
        if process.poll() is not None:
            return
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            self.fail("Browser server did not shut down")
        self.assertEqual(process.returncode, 0)

    def session(self, page=None):
        if page is None:
            page = self.context.new_page()
            page.goto(self.origin)
        previous = page.url
        page.click("#new-session")
        page.wait_for_url(lambda url: str(url) != previous and re.fullmatch(r".*/sessions/[a-f0-9]{32}", str(url)))
        expect(page.locator("#model")).to_be_enabled()
        return page

    def submit(self, page, text):
        page.fill("#prompt", text)
        page.press("#prompt", "Enter")
        expect(page.locator(".user").last).to_contain_text(text)

    def test_sky_city_drives_altitude_layers_and_persists_across_navigation(self):
        page = self.page
        calls = []
        def weather(route):
            calls.append(route.request.url)
            route.fulfill(json={"utc_offset_seconds": 0, "current": {
                "time": int(time.time()), "cloud_cover_low": 0, "cloud_cover_mid": 65,
                "cloud_cover_high": 90, "wind_speed_10m": 6, "wind_direction_10m": 250}})
        self.context.route("**/api/sky/weather?*", weather)
        self.context.route("**/api/sky/locations?*", lambda route: route.fulfill(json={"results": [
            {"name": "London", "admin1": "England", "country": "United Kingdom", "latitude": 51.5085, "longitude": -0.1257}]}))
        page.reload()
        expect(page.locator("#sky")).to_have_attribute("data-weather", "illustrated")
        expect(page.locator("#sky")).to_have_attribute("data-clouds", "ready", timeout=30000)
        self.assertEqual(calls, [], "Illustrated skies must not request a location or weather")
        page.emulate_media(reduced_motion="reduce")
        page.click("#sky-toggle")
        page.fill("#sky-city", "London")
        page.press("#sky-city", "Enter")
        page.get_by_role("button", name="London, England, United Kingdom", exact=True).click()
        expect(page.locator("#sky")).to_have_attribute("data-weather", "live")
        expect(page.locator("#sky-low")).to_have_text("0%")
        self.assertIn("latitude=51.51&longitude=-0.13", calls[-1])
        for layer, cover in [("high", "90"), ("mid", "65"), ("low", "0")]:
            expect(page.locator(f".cloud-{layer}")).to_have_attribute("data-cover", cover)
        self.assertTrue(page.locator(".cloud-low .cloud-sprite").evaluate_all("nodes => nodes.every(n => getComputedStyle(n).opacity === '0')"))
        self.assertTrue(page.locator(".cloud-high .cloud-sprite").evaluate_all("nodes => nodes.some(n => getComputedStyle(n).opacity === '1')"))
        page.press("#sky-city", "Escape")
        self.session(page)
        expect(page.locator("#sky")).to_have_attribute("data-weather", "live")
        page.click("#sky-toggle")
        expect(page.locator("#sky-status")).to_contain_text("London")
        page.click("#sky-reset")
        expect(page.locator("#sky")).to_have_attribute("data-weather", "illustrated")
        page.reload()
        expect(page.locator("#sky")).to_have_attribute("data-weather", "illustrated")
        self.assertIsNone(page.evaluate("localStorage.getItem('myco.sky.location.v1')"))

    def test_sky_weather_outages_mark_old_conditions_and_eventually_use_illustration(self):
        page = self.page
        page.clock.install()
        self.context.route("**/api/sky/weather?*", lambda route: route.fulfill(json={
            "utc_offset_seconds": 0, "current": {"time": int(time.time()), "cloud_cover_low": 100,
            "cloud_cover_mid": 100, "cloud_cover_high": 0, "wind_speed_10m": 12, "wind_direction_10m": 45,
            "interval": 900, "rain": 1, "showers": 0, "weather_code": 63}}))
        page.evaluate("localStorage.setItem('myco.sky.location.v1', JSON.stringify({name: 'Test city', latitude: 50, longitude: 0}))")
        page.reload()
        expect(page.locator("#sky")).to_have_attribute("data-weather", "live")
        self.context.unroute("**/api/sky/weather?*")
        self.context.route("**/api/sky/weather?*", lambda route: route.fulfill(status=503, body="Weather upstream unavailable"))
        page.clock.fast_forward(15 * 60 * 1000)
        expect(page.locator("#sky")).to_have_attribute("data-weather", "stale")
        expect(page.locator("#sky-rain")).to_be_visible()
        page.click("#sky-toggle")
        expect(page.locator("#sky-status")).to_contain_text("last available")
        expect(page.locator("#sky-error")).to_contain_text("Weather unavailable")
        page.clock.fast_forward(2 * 60 * 60 * 1000)
        expect(page.locator("#sky")).to_have_attribute("data-weather", "illustrated")
        expect(page.locator("#sky-coverage")).to_be_hidden()
        expect(page.locator("#sky-rain")).to_be_hidden()
        page.click("#sky-close")
        self.session(page)
        self.submit(page, "Alpha markdown")
        expect(page.locator("#model")).to_be_enabled()

    def test_sky_motion_preferences_mobile_settings_and_location_denial(self):
        page = self.page
        page.set_viewport_size({"width": 390, "height": 844})
        page.emulate_media(reduced_motion="reduce")
        self.assertEqual(page.locator(".cloud-track").first.evaluate("n => getComputedStyle(n).animationName"), "none")
        page.emulate_media(reduced_motion="no-preference")
        self.assertGreater(float(page.locator(".cloud-track").first.evaluate("n => parseFloat(getComputedStyle(n).animationDuration)")), 1000)
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: true}); document.dispatchEvent(new Event('visibilitychange'))")
        self.assertEqual(page.locator(".cloud-track").first.evaluate("n => getComputedStyle(n).animationPlayState"), "paused")
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: false}); document.dispatchEvent(new Event('visibilitychange'))")
        page.click("#sky-toggle")
        page.evaluate("() => { navigator.geolocation.getCurrentPosition = (_ok, fail) => fail({code: 1}); }")
        page.click("#sky-locate")
        expect(page.locator("#sky-error")).to_contain_text("Location unavailable")
        self.assertLessEqual(page.evaluate("document.documentElement.scrollWidth"), 390)
        rect = page.locator("#sky-settings").bounding_box()
        self.assertGreaterEqual(rect["x"], 0)
        self.assertLessEqual(rect["x"] + rect["width"], 390)
        page.screenshot(path=str(self.artifacts / "sky-settings-mobile.png"))
        page.click("#sky-close")
        self.session(page)
        self.assertLessEqual(page.evaluate("document.documentElement.scrollWidth"), 390)

    def test_sky_endpoints_require_authentication_and_validate_input(self):
        anonymous = self.playwright.request.new_context()
        try:
            for path in ["/api/sky/weather?latitude=0&longitude=0", "/api/sky/locations?query=London", "/clouds.js", "/cloud-renderer.js", "/aircraft.js", "/rain.js"]:
                self.assertEqual(anonymous.get(self.origin + path).status, 401)
        finally:
            anonymous.dispose()
        for path in ["/api/sky/weather?latitude=91&longitude=0", "/api/sky/weather?latitude=nan&longitude=0", "/api/sky/locations?query=a"]:
            self.assertEqual(self.context.request.get(self.origin + path).status, 400)

    def test_sky_rain_tracks_weather_intensity_and_clears_for_snow_and_illustration(self):
        page = self.page
        page.clock.install()
        report = {"utc_offset_seconds": 0, "current": {"time": int(time.time()), "interval": 900,
            "cloud_cover_low": 95, "cloud_cover_mid": 90, "cloud_cover_high": 40,
            "wind_speed_10m": 6, "wind_direction_10m": 270, "rain": 0.5, "showers": 0.25, "weather_code": 63}}
        self.context.route("**/api/sky/weather?*", lambda route: route.fulfill(json=report))
        page.evaluate("localStorage.setItem('myco.sky.location.v1', JSON.stringify({name: 'Test city', latitude: 50, longitude: 0}))")
        page.reload()
        rain = page.locator("#sky-rain")
        expect(rain).to_be_visible()
        page.click("#sky-toggle")
        expect(page.locator("#sky-conditions")).to_have_text("Rain")
        sheet = rain.locator(".rain-sheet").first
        opacity = sheet.evaluate("n => getComputedStyle(n).opacity")
        self.assertIn("data:image/png", sheet.evaluate("n => getComputedStyle(n).backgroundImage"))
        report["current"]["interval"] = 3600
        page.clock.fast_forward(15 * 60 * 1000)
        expect(sheet).not_to_have_css("opacity", opacity)
        self.assertLess(float(sheet.evaluate("n => getComputedStyle(n).opacity")), float(opacity))
        report["current"].update(rain=5, weather_code=65, interval=900)
        page.clock.fast_forward(15 * 60 * 1000)
        expect(page.locator("#sky-conditions")).to_have_text("Heavy rain")
        self.assertGreater(float(sheet.evaluate("n => getComputedStyle(n).opacity")), float(opacity))
        report["current"].update(rain=0, showers=0, weather_code=73)
        page.clock.fast_forward(15 * 60 * 1000)
        expect(page.locator("#sky-conditions")).to_have_text("Snow")
        expect(rain).to_be_hidden()
        report["current"]["weather_code"] = 51
        page.clock.fast_forward(15 * 60 * 1000)
        expect(page.locator("#sky-conditions")).to_have_text("Light drizzle")
        expect(rain).to_be_visible()
        page.click("#sky-reset")
        expect(rain).to_be_hidden()
        expect(page.locator("#sky-conditions")).to_be_hidden()

    def test_sky_rain_respects_reduced_motion_and_pauses_in_hidden_tabs(self):
        page = self.page
        self.context.route("**/api/sky/weather?*", lambda route: route.fulfill(json={
            "utc_offset_seconds": 0, "current": {"time": int(time.time()), "interval": 900,
            "cloud_cover_low": 100, "cloud_cover_mid": 100, "cloud_cover_high": 20,
            "wind_speed_10m": 8, "wind_direction_10m": 250, "rain": 2, "showers": 0, "weather_code": 65}}))
        page.evaluate("localStorage.setItem('myco.sky.location.v1', JSON.stringify({name: 'Test city', latitude: 50, longitude: 0}))")
        page.reload()
        rain = page.locator("#sky-rain")
        expect(rain).to_be_visible()
        self.assertEqual(rain.evaluate("n => getComputedStyle(n).pointerEvents"), "none")
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: true}); document.dispatchEvent(new Event('visibilitychange'))")
        self.assertTrue(rain.locator(".rain-sheet").evaluate_all("nodes => nodes.every(n => getComputedStyle(n).animationPlayState === 'paused')"))
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: false}); document.dispatchEvent(new Event('visibilitychange'))")
        self.assertTrue(rain.locator(".rain-sheet").evaluate_all("nodes => nodes.every(n => getComputedStyle(n).animationPlayState === 'running')"))
        page.emulate_media(reduced_motion="reduce")
        expect(rain).to_be_hidden()
        expect(page.locator("#sky")).to_have_attribute("data-weather", "live")
        page.click("#sky-toggle")
        expect(page.locator("#sky-conditions")).to_have_text("Heavy rain")
        page.click("#sky-close")
        page.emulate_media(reduced_motion="no-preference")
        expect(rain).to_be_visible()
        self.session(page)
        self.submit(page, "Alpha markdown")
        expect(page.locator(".assistant .markdown table")).to_be_visible()

    def test_sky_wind_changes_preserve_cloud_positions(self):
        page = self.page
        page.clock.install()
        report = {"utc_offset_seconds": 0, "current": {"time": int(time.time()),
            "cloud_cover_low": 80, "cloud_cover_mid": 50, "cloud_cover_high": 70,
            "wind_speed_10m": 4, "wind_direction_10m": 250}}
        self.context.route("**/api/sky/weather?*", lambda route: route.fulfill(json=report))
        page.evaluate("localStorage.setItem('myco.sky.location.v1', JSON.stringify({name: 'Test city', latitude: 50, longitude: 0}))")
        page.reload()
        expect(page.locator("#sky")).to_have_attribute("data-weather", "live")
        tracks = page.locator(".cloud-track")
        tracks.evaluate_all("nodes => nodes.forEach(n => { n.getAnimations()[0].currentTime = 1200000; })")
        before = tracks.evaluate_all("nodes => nodes.map(n => n.getBoundingClientRect().x)")
        report["current"]["wind_direction_10m"] = 90
        report["current"]["wind_speed_10m"] = 15
        report["current"]["cloud_cover_low"] = 85
        page.clock.fast_forward(15 * 60 * 1000)
        expect(page.locator(".cloud-low")).to_have_attribute("data-cover", "85")
        after = tracks.evaluate_all("nodes => nodes.map(n => n.getBoundingClientRect().x)")
        for start, end in zip(before, after):
            self.assertLess(abs(end - start), 2, "Weather updates must not jump drifting clouds")

    def test_sky_renderer_failure_keeps_the_fallback_and_conversation_usable(self):
        self.context.route("**/cloud-renderer.js", lambda route: route.fulfill(status=503, body="Unavailable"))
        page = self.page
        page.reload()
        expect(page.locator("#sky")).to_have_attribute("data-clouds", "fallback")
        self.session(page)
        self.submit(page, "Alpha markdown")
        expect(page.locator(".assistant .markdown table")).to_be_visible()
        expect(page.locator("#model")).to_be_enabled()

    def test_sky_airplanes_arrive_occasionally_stay_bounded_and_leave(self):
        page = self.page
        page.clock.install()
        page.reload()
        planes = page.locator(".sky-aircraft")
        page.clock.fast_forward(30000)
        expect(planes).to_have_count(0)
        page.clock.fast_forward(81000)
        expect(planes.first).to_be_attached()
        for _ in range(6):
            page.clock.fast_forward(361000)
            self.assertLessEqual(planes.count(), 2)
        self.assertEqual(page.locator("#sky-aircraft").evaluate("n => getComputedStyle(n).pointerEvents"), "none")
        planes.evaluate_all("nodes => nodes.forEach(n => n.getAnimations().forEach(a => a.finish()))")
        expect(planes).to_have_count(0)

    def test_sky_airplanes_pause_when_hidden_and_stop_for_reduced_motion(self):
        page = self.page
        page.clock.install()
        page.emulate_media(reduced_motion="reduce")
        page.reload()
        planes = page.locator(".sky-aircraft")
        page.clock.fast_forward(3600000)
        expect(planes).to_have_count(0)
        # Media changes are delivered asynchronously, before the arrival timer starts.
        page.evaluate("() => { window.motionChanged = new Promise(resolve => matchMedia('(prefers-reduced-motion: reduce)').addEventListener('change', () => resolve(), {once: true})); }")
        page.emulate_media(reduced_motion="no-preference")
        page.evaluate("window.motionChanged")
        page.clock.fast_forward(111000)
        expect(planes.first).to_be_attached()
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: true}); document.dispatchEvent(new Event('visibilitychange'))")
        self.assertEqual(planes.first.evaluate("n => getComputedStyle(n).animationPlayState"), "paused")
        count = planes.count()
        elapsed = planes.first.evaluate("n => n.getAnimations()[0].currentTime")
        page.clock.fast_forward(3600000)
        expect(planes).to_have_count(count)
        self.assertAlmostEqual(planes.first.evaluate("n => n.getAnimations()[0].currentTime"), elapsed, delta=1)
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: false}); document.dispatchEvent(new Event('visibilitychange'))")
        self.assertEqual(planes.first.evaluate("n => getComputedStyle(n).animationPlayState"), "running")
        page.emulate_media(reduced_motion="reduce")
        expect(planes).to_have_count(0)
        page.clock.fast_forward(3600000)
        expect(planes).to_have_count(0)

    def test_refresh_keeps_transcript_nodes_and_expanded_tools(self):
        page = self.session(self.page)
        self.submit(page, "Alpha shell")
        expect(page.locator(".tool.done")).to_have_count(1)
        expect(page.locator("#model")).to_be_enabled()
        page.locator(".tool summary").click()
        node = page.locator(".tool").element_handle()
        self.submit(page, "Alpha markdown")
        expect(page.locator(".markdown table")).to_have_count(1)
        expect(page.locator("#model")).to_be_enabled()
        body = page.locator(".markdown").last.element_handle()
        page.select_option("#model", "second")
        expect(page.locator("#model")).to_be_enabled()
        expect(page.locator("#model")).to_have_value("second")
        self.assertTrue(node.evaluate("node => node.isConnected"), "Unchanged tools must keep their DOM nodes")
        self.assertTrue(body.evaluate("node => node.isConnected"), "Metadata snapshots must retain rendered Markdown")
        expect(page.locator(".tool")).to_have_attribute("open", "")

    def test_queued_messages_survive_refresh_and_join_the_next_tool_results_in_order(self):
        page = self.session(self.page)
        self.submit(page, "Alpha wait")
        expect(page.locator(".tool.running")).to_have_count(1)
        expect(page.locator("#send")).to_have_text("Queue ↵")
        for text in ["Alpha markdown", "Alpha stream"]:
            page.fill("#prompt", text)
            page.press("#prompt", "Enter")
            expect(page.locator("#prompt")).to_have_value("")
        expect(page.locator("#queued-list li")).to_have_text(["Alpha markdown", "Alpha stream"])
        expect(page.locator(".user")).to_have_count(1)
        page.screenshot(path=str(self.artifacts / "queued-messages.png"))
        page.reload()
        expect(page.locator("#queued-list li")).to_have_text(["Alpha markdown", "Alpha stream"])
        (self.home / "Alpha-release").touch()
        expect(page.locator(".user .body")).to_have_text(["Alpha wait", "Alpha markdown", "Alpha stream"])
        expect(page.locator("#model")).to_be_enabled()
        expect(page.locator("#queued")).to_be_hidden()
        expect(page.locator(".markdown p").last).to_have_text("Chunk 29.")
        self.assertEqual(len(self.requests), 2, "Follow-ups share the next model request after the tool result")
        self.assertEqual(self.turns.get("Alpha wait"), 1)
        self.assertNotIn("Alpha markdown", self.turns, "There is no separate model request for each queued message")
        request = self.requests[-1]["input"]
        result = next(i for i, item in enumerate(request) if item.get("type") == "function_call_output")
        followups = [i for i, item in enumerate(request) if item.get("role") == "user" and "Alpha markdown" in json.dumps(item)]
        self.assertTrue(followups and all(i > result for i in followups))

    def test_tool_timers_tick_every_tenth_and_keep_independent_durations_across_refresh(self):
        page = self.page
        page.clock.install(time="2000-01-01T00:00:00Z")
        self.session(page)
        self.submit(page, "Alpha parallel")
        expect(page.locator(".tool.running")).to_have_count(2)
        expect(page.locator(".tool.running .tool-duration")).to_have_text([re.compile(r"^[1-9]\d*\.\ds$"), re.compile(r"^[1-9]\d*\.\ds$")])
        before = [float(text[:-1]) for text in page.locator(".tool-duration").all_text_contents()[:2]]
        self.assertTrue(all(1 <= duration < 30 for duration in before), "Browser clock skew must not affect elapsed time")
        page.click("#activity-toggle")
        page.clock.pause_at(page.evaluate("Date.now() / 1000 + 1"))
        page.clock.run_for(100)
        timers = page.locator(".tool-duration[data-running=true]")
        previous = [float(text[:-1]) for text in timers.all_text_contents()]
        self.assertEqual(len(previous), 4, "Both the tool headers and activity drawer show timers")
        for _ in range(3):
            page.clock.run_for(100)
            current = [float(text[:-1]) for text in timers.all_text_contents()]
            self.assertEqual(current[:2], current[2:], "Header and activity timers agree")
            for old, new in zip(previous, current):
                self.assertAlmostEqual(new - old, 0.1)
            previous = current
        page.clock.resume()
        page.reload()
        expect(page.locator(".tool.running")).to_have_count(2)
        after = [float(text[:-1]) for text in page.locator(".tool .tool-duration").all_text_contents()]
        self.assertTrue(all(new >= old for old, new in zip(before, after)), "Refreshing must not reset a running timer")
        (self.home / "Alpha-release-0").touch()
        expect(page.locator(".tool.done")).to_have_count(1)
        finished = page.locator(".tool.done .tool-duration").inner_text()
        running_timer = page.locator(".tool.running .tool-duration")
        running = float(running_timer.inner_text()[:-1])
        for _ in range(3):
            expect(running_timer).not_to_have_text(running_timer.inner_text())
        self.assertGreaterEqual(float(running_timer.inner_text()[:-1]) - running, 0.29)
        expect(page.locator(".tool.done .tool-duration")).to_have_text(finished)
        page.screenshot(path=str(self.artifacts / "independent-tool-timers.png"))
        page.click("#cancel")
        expect(page.locator("#model")).to_be_enabled()
        expect(page.locator(".tool.failed")).to_have_count(1)
        expect(page.locator(".tool.done .tool-duration")).to_have_text(finished)
        stopped = page.locator(".tool .tool-duration").all_text_contents()
        self.submit(page, "Alpha markdown")
        expect(page.locator("#model")).to_be_enabled()
        expect(page.locator(".tool .tool-duration")).to_have_text(stopped)
        page.reload()
        expect(page.locator(".tool .tool-duration")).to_have_text(stopped)

    def test_cancel_sends_queued_messages_with_the_cancelled_tool_results(self):
        page = self.session(self.page)
        self.submit(page, "Alpha wait")
        expect(page.locator(".tool.running")).to_have_count(1)
        page.fill("#prompt", "Alpha markdown")
        page.press("#prompt", "Enter")
        expect(page.locator("#queued-list li")).to_have_text(["Alpha markdown"])
        expect(page.locator("#cancel")).to_have_text("Cancel & send queued")
        page.click("#cancel")
        expect(page.locator("#model")).to_be_enabled()
        expect(page.locator("#queued")).to_be_hidden()
        expect(page.locator(".user .body")).to_have_text(["Alpha wait", "Alpha markdown"])
        expect(page.locator(".tool.failed")).to_have_count(1)
        expect(page.locator(".markdown table")).to_have_count(1)
        self.assertEqual(self.turns.get("Alpha markdown"), 1)
        self.assertEqual(len(self.requests), 2)
        results = [item for item in self.requests[-1]["input"] if item.get("type") == "function_call_output"]
        self.assertEqual(len(results), 1)
        self.assertIn("cancel", json.dumps(results).lower())

    def test_queued_messages_wait_for_all_results_in_a_parallel_tool_batch(self):
        page = self.session(self.page)
        self.submit(page, "Alpha parallel")
        expect(page.locator(".tool.running")).to_have_count(2)
        page.fill("#prompt", "Alpha markdown")
        page.press("#prompt", "Enter")
        expect(page.locator("#queued-list li")).to_have_text(["Alpha markdown"])
        (self.home / "Alpha-release-0").touch()
        expect(page.locator(".tool.done")).to_have_count(1)
        page.wait_for_timeout(150)
        expect(page.locator("#queued-list li")).to_have_text(["Alpha markdown"])
        self.assertEqual(len(self.requests), 1)
        (self.home / "Alpha-release-1").touch()
        expect(page.locator("#model")).to_be_enabled()
        expect(page.locator(".user .body")).to_have_text(["Alpha parallel", "Alpha markdown"])
        self.assertEqual(len(self.requests), 2)
        results = [item for item in self.requests[-1]["input"] if item.get("type") == "function_call_output"]
        self.assertEqual(len(results), 2)

    def test_titles_update_during_turns_and_tool_inputs_use_labeled_fields(self):
        page = self.session(self.page)
        self.submit(page, "Alpha wait")
        expect(page.locator(".tool.running")).to_have_count(1)
        expect(page.locator("#session-title")).to_have_text("Alpha wait")
        expect(page).to_have_title("Alpha wait · myco")
        page.locator(".tool summary").click()
        fields = page.locator(".tool .arguments")
        expect(fields.locator("dt strong")).to_have_text(["command", "host", "timeout_ms"])
        expect(fields.locator("dd").nth(1)).to_have_text("local")
        self.assertIn("\nprintf done", fields.locator("dd pre").first.inner_text())
        self.assertNotIn('"command":', page.locator(".tool summary").inner_text())
        page.screenshot(path=str(self.artifacts / "tool-inputs.png"))
        (self.home / "Alpha-release").touch()
        expect(page.locator("#model")).to_be_enabled()
        self.submit(page, "Alpha rename")
        expect(page.locator(".tool.running")).to_have_count(1)
        expect(page.locator("#session-title")).to_have_text("Renamed Alpha")
        expect(page).to_have_title("Renamed Alpha · myco")
        home = self.context.new_page()
        home.goto(self.origin)
        expect(home.locator(".session-name")).to_have_text("Renamed Alpha")
        page.reload()
        expect(page).to_have_title("Renamed Alpha · myco")
        expect(page.locator(".tool.running")).to_have_count(1)
        (self.home / "Alpha-rename-release").touch()
        expect(page.locator("#model")).to_be_enabled()

    def test_home_refresh_keeps_links_and_focus(self):
        self.session(self.page)
        page = self.context.new_page()
        page.goto(self.origin)
        expect(page.locator("#session-list a")).to_have_count(1)
        link = page.locator("#session-list a").element_handle()
        link.focus()
        with page.expect_response("**/api/sessions?*"):
            page.evaluate("window.dispatchEvent(new PageTransitionEvent('pageshow'))")
        page.wait_for_timeout(100)
        self.assertTrue(link.evaluate("node => node.isConnected"), "Unchanged rows must survive refresh")
        expect(page.locator("#session-list a")).to_be_focused()
        page.fill("#search", "no-such-session")
        expect(page.locator("#session-list a:visible")).to_have_count(0)
        page.fill("#search", "")
        expect(page.locator("#session-list a:visible")).to_have_count(1)

    def test_failed_markdown_render_retries_on_the_next_snapshot(self):
        page = self.session(self.page)
        page.route("**/api/markdown", lambda route: route.abort(), times=1)
        with page.expect_event("requestfailed", predicate=lambda request: request.url.endswith("/api/markdown")):
            self.submit(page, "Alpha markdown")
        expect(page.locator("#model")).to_be_enabled()
        body = page.locator(".markdown").element_handle()
        page.select_option("#model", "second")
        expect(page.locator(".markdown table")).to_have_count(1)
        self.assertTrue(body.evaluate("node => node.isConnected"))

    def test_streaming_coalesces_markdown_requests(self):
        page = self.session(self.page)
        expect(page.locator(".notice")).to_contain_text("prelude directory unreadable")
        held = []
        self.addCleanup(lambda: [route.abort() for route in held])
        page.route("**/api/markdown", lambda route: held.append(route))
        self.submit(page, "Alpha stream")
        expect(page.locator("#model")).to_be_enabled()
        page.wait_for_timeout(200)
        self.assertEqual(len(held), 1, "Only one render of the streaming block may be in flight")
        first = held.pop()
        first.fulfill(response=first.fetch())
        page.wait_for_timeout(150)
        self.assertEqual(len(held), 1, "Render the latest text after the outstanding render finishes")
        last = held.pop()
        last.fulfill(response=last.fetch())
        expect(page.locator(".markdown p").last).to_have_text("Chunk 29.")
        page.wait_for_timeout(150)
        self.assertEqual(held, [])

    def test_sessions_isolate_models_tools_and_cancellation(self):
        alpha = self.session(self.page)
        beta = self.session()
        beta.select_option("#model", "second")
        expect(beta.locator("#model")).to_be_enabled()
        for page, name in [(alpha, "Alpha"), (beta, "Beta")]:
            self.submit(page, f"{name} shell")
            expect(page.locator("#background-list li")).to_have_count(1)
            expect(page.locator("#model")).to_be_enabled()
            self.submit(page, f"{name} wait")
            expect(page.locator(".tool.running")).to_have_count(1)
            expect(page.locator("#model")).to_be_disabled()
        duplicate = self.context.new_page()
        duplicate.goto(alpha.url)
        expect(duplicate.locator(".tool.running")).to_have_count(1)
        expect(duplicate.locator("#model")).to_have_value("first")
        beta.click("#cancel")
        expect(beta.locator(".tool.failed")).to_have_count(1)
        expect(beta.locator("#model")).to_be_enabled()
        expect(alpha.locator(".tool.running")).to_have_count(1)
        (self.home / "Alpha-release").touch()
        for page in [alpha, duplicate]:
            expect(page.locator("#model")).to_be_enabled()
            expect(page.locator(".tool.done")).to_have_count(2)
        self.assertTrue((self.home / "Alpha-done").exists())
        self.assertFalse((self.home / "Beta-done").exists())
        for page, name in [(alpha, "Alpha"), (beta, "Beta")]:
            self.submit(page, f"{name} read marker")
            expect(page.locator(".tool")).to_have_count(3)
            expect(page.locator("#model")).to_be_enabled()
            self.assertEqual((self.home / f"{name}-marker").read_text(), name)
        self.assertEqual({request["model"] for request in self.requests}, {"first", "second"})

    def test_tab_history_and_shared_connection_survive_restart(self):
        alpha = self.session(self.page)
        original = alpha.url
        self.submit(alpha, "Alpha wait")
        expect(alpha.locator(".tool.running")).to_have_count(1)
        tabs = [self.session() for _ in range(8)]
        alpha.close()
        (self.home / "Alpha-release").touch()
        alpha = self.context.new_page()
        alpha.goto(original)
        expect(alpha.locator(".tool.done")).to_have_count(1)
        expect(alpha.locator("#model")).to_be_enabled()
        targets = self.context.new_cdp_session(alpha).send("Target.getTargets")["targetInfos"]
        self.assertEqual(len([t for t in targets if t["type"] == "shared_worker" and t["url"].startswith(self.origin)]), 1)
        back = tabs[0]
        previous = back.url
        self.session(back)
        first_new = back.url
        back.go_back()
        expect(back.locator("#model")).to_be_enabled()
        self.assertEqual(back.url, previous)
        self.session(back)
        self.assertNotEqual(back.url, first_new)
        requests = len(self.requests)
        node = alpha.locator(".tool").element_handle()
        self.stop(self.process)
        self.process, launch = self.launch(urlsplit(self.origin).port, original.rsplit("/", 1)[1])
        restored = self.context.new_page()
        restored.goto(launch)
        expect(restored.locator(".tool.done")).to_have_count(1)
        self.assertEqual(restored.url, original)
        for page in [alpha, *tabs]:
            expect(page.locator("#model")).to_be_enabled(timeout=15000)
        self.assertTrue(node.evaluate("node => node.isConnected"))
        self.assertEqual(len(self.requests), requests)

    def test_markdown_images_and_floating_controls(self):
        page = self.session(self.page)
        self.submit(page, "Alpha markdown")
        expect(page.locator(".markdown table")).to_have_count(1)
        expect(page.locator('.markdown img')).to_have_js_property('naturalWidth', 1)
        self.assertEqual(page.locator(".markdown h1").evaluate("n => getComputedStyle(n).fontSize"),
                         page.locator(".markdown p").first.evaluate("n => getComputedStyle(n).fontSize"))
        self.assertEqual(page.locator("#composer time").count(), 0)
        expect(page.locator("#activity")).to_be_hidden()
        page.click("#activity-toggle")
        expect(page.locator("#activity-close")).to_be_focused()
        page.keyboard.press("Escape")
        expect(page.locator("#activity-toggle")).to_be_focused()
        page.evaluate("window.scrollTo(0, 0)")
        expect(page.locator("#jump")).to_be_visible()
        expect(page.locator("#composer")).to_be_in_viewport()

        page.click("#jump")
        expect(page.locator("#jump")).to_be_hidden()
        page.set_viewport_size({"width": 390, "height": 844})
        self.assertTrue(page.evaluate("document.documentElement.scrollWidth <= innerWidth"))
        expect(page.locator("#composer")).to_be_in_viewport()
    def test_archive_preserves_live_work_and_restores_after_restart(self):
        session = self.session(self.page)
        url = session.url
        self.submit(session, "Alpha wait")
        expect(session.locator(".tool.running")).to_have_count(1)
        home = self.context.new_page()
        home.goto(self.origin)
        home.get_by_role("button", name=re.compile("^Archive ")).click()
        expect(home.locator("#session-list li")).to_have_count(0)
        expect(session.locator(".tool.running")).to_have_count(1)
        home.select_option("#archive-filter", "archived")
        expect(home.locator("#session-list a")).to_have_attribute("href", urlsplit(url).path)
        (self.home / "Alpha-release").touch()
        expect(session.locator(".tool.done")).to_have_count(1)
        expect(session.locator("#model")).to_be_enabled()
        requests = len(self.requests)
        self.stop(self.process)
        self.process, launch = self.launch(urlsplit(self.origin).port)
        home.goto(launch)
        expect(home.locator("#session-list li")).to_have_count(0)
        home.select_option("#archive-filter", "archived")
        expect(home.locator("#session-list a")).to_have_attribute("href", urlsplit(url).path)
        home.get_by_role("button", name=re.compile("^Restore ")).click()
        expect(home.locator("#session-list li")).to_have_count(0)
        home.select_option("#archive-filter", "active")
        expect(home.locator("#session-list a")).to_have_attribute("href", urlsplit(url).path)
        home.locator("#session-list a").click()
        expect(home.locator(".tool.done")).to_have_count(1)
        expect(home.locator("#model")).to_be_enabled()
        self.assertEqual(len(self.requests), requests)



if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=lambda value: Path(value).resolve(), default=Path("target/debug/myco").resolve())
    parser.add_argument("--browser", help="Chromium executable; defaults to Playwright's installed browser")
    parser.add_argument("--artifacts", type=Path, default=Path("target/browser-test-results"))
    OPTIONS, remaining = parser.parse_known_args()
    unittest.main(argv=[__file__, *remaining], verbosity=2)
