"""Exercise the embedded UI in Chromium with real tools and a local scripted model."""

import argparse
import base64
from datetime import datetime, timedelta
import http.server
import json
import os
from pathlib import Path
import re
import selectors
import shlex
import signal
import socket
import socketserver
import subprocess
import tempfile
import threading
import time
import unittest
from urllib.parse import urlsplit
import uuid
from zoneinfo import ZoneInfo

from playwright.sync_api import expect, sync_playwright


def reply(text):
    parts = text if isinstance(text, list) else [text]
    return [{"type": "response.output_text.delta", "output_index": index,
             "content_index": 0, "delta": part} for index, part in enumerate(parts)]


def tool(arguments, name="bash", index=0):
    return [
        {"type": "response.output_item.added", "output_index": index,
         "item": {"type": "function_call", "name": name,
                  "call_id": str(uuid.uuid4()), "arguments": ""}},
        {"type": "response.function_call_arguments.done", "output_index": index,
         "arguments": json.dumps(arguments)},
    ]


def compaction_reply(body):
    for spec in body.get('tools', []):
        fields = spec.get('parameters', {}).get('properties', {})
        if spec.get('name') != 'session_history' or 'const' not in fields.get('session_id', {}):
            continue
        if any(item.get('type') == 'function_call_output' for item in body['input']):
            return reply('Summary saved.')
        return tool({'action': 'write_summary', 'session_id': fields['session_id']['const'],
                     'thread_id': fields['thread_id']['const'], 'markdown': 'Continue the browser fixture task.'},
                    'session_history')
    return None


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        fixture = self.server.fixture
        fixture.requests.append(body)
        fixture.request_received.set()
        prompts = []
        resuming = False
        for item in body["input"]:
            if item.get("role") != "user":
                continue
            content = item["content"]
            if not isinstance(content, str):
                content = " ".join(part.get("text", "") for part in content)
            resuming |= '# Resumption\n\n' in content
            match = re.search(r"(Alpha|Beta) (shell|read marker|wait|stream|markdown|rename|parallel|generate|fail|images|links|profile)\b", content)
            if match:
                prompts.append(match.group(0))
        prompt = prompts[-1] if prompts else 'Alpha images'
        count = fixture.turns.get(prompt, 0)
        fixture.turns[prompt] = count + 1
        name = "Alpha" if "Alpha" in prompt else "Beta"
        root = fixture.home
        compact = compaction_reply(body)
        if compact is not None:
            events = compact
        elif getattr(fixture, 'reject_payload', lambda _body: False)(body):
            self.send_error(413, 'Fixture payload too large')
            return
        elif "fail" in prompt:
            self.send_error(400, "Fixture model failure")
            return
        elif count and "links" in prompt:
            events = reply(fixture.link_reply)
        elif count == 1 and "rename" in prompt:
            release = shlex.quote(str(root / (name + "-rename-release")))
            events = tool({"command": f"while [ ! -f {release} ]; do sleep 0.05; done", "timeout_ms": 60000})
        elif "stream" in prompt and (not count or hasattr(fixture, 'stream_chunks')):
            events = [event for i in range(getattr(fixture, 'stream_chunks', 30)) for event in reply(f"Chunk {i}.\n\n")]
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
        elif "links" in prompt:
            output = f'Preview: {fixture.origin}/files/linked.txt?from=tool&mode=full#details.'
            events = tool({'command': f"printf '%s\\n' {shlex.quote(output)}", 'timeout_ms': 1000})
        elif "profile" in prompt:
            marker = shlex.quote(str(root / (name + '-profile')))
            events = tool({'command': f'printf "%s" "$MYCO_PROFILE" > profile-tool.txt\nprintf "%s\\n" "$MYCO_PROFILE" "$MYCO_HOME" "$PWD" "$MYCO_SERVER_URL" > {marker}; cat {marker}', 'timeout_ms': 1000})
        elif "generate" in prompt:
            events = reply(f"{name} finished.")
        else:
            events = reply("# Heading\n\n**Bold** and _emphasis_.\n\n"
                           "| Name | Value |\n| --- | --- |\n| Tool | Ready |\n\n"
                           f"![Local image]({root / 'pixel.png'})\n\n"
                           + "\n\n".join(f"Paragraph {i}: session output." for i in range(24)))
        events.append({"type": "response.completed", "response": {
            "status": "completed", "usage": getattr(fixture, 'usage', {"input_tokens": 100, "output_tokens": 20})}})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            if compact is not None:
                fixture.compaction_release.wait(30)
            elif resuming:
                fixture.continuation_release.wait(30)
            if "generate" in prompt:
                fixture.generation_releases[name].wait(30)
            for index, event in enumerate(events):
                if gate := getattr(fixture, 'event_gates', {}).get(index):
                    gate.wait(30)
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
        workspace = self.workspace = self.home / "profiles/default/workspace"
        workspace.mkdir(parents=True)
        (workspace / "prelude").touch()
        self.artifacts = OPTIONS.artifacts / self._testMethodName
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.requests = []
        self.request_received = threading.Event()
        self.turns = {}
        self.processes = []
        self.errors = []
        provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        provider.fixture = self
        self.addCleanup(provider.server_close)
        self.addCleanup(provider.shutdown)
        self.generation_releases = {name: threading.Event() for name in ['Alpha', 'Beta']}
        self.addCleanup(lambda: [gate.set() for gate in self.generation_releases.values()])
        self.compaction_release = threading.Event()
        self.compaction_release.set()
        self.addCleanup(self.compaction_release.set)
        self.continuation_release = threading.Event()
        self.continuation_release.set()
        self.addCleanup(self.continuation_release.set)
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        (self.home / "config.toml").write_text('model = "first"\n' + "".join(f'''
[models.{name}]
protocol = "openai-responses"
base_url = "http://127.0.0.1:{provider.server_port}"
auth = {{ source = "none" }}
context_window = 100000
''' for name in ["first", "second"]))
        (self.home / "pixel.png").write_bytes(base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6cGkAAAAASUVORK5CYII="))
        (self.workspace / "pixel.png").write_bytes((self.home / "pixel.png").read_bytes())
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

    def launch(self, port=0, resume=None, profile='default', config='config.toml'):
        log = (self.artifacts / f"server-{len(self.processes)}.log").open("w")
        self.addCleanup(log.close)
        args = [str(OPTIONS.binary), "--port", str(port), "--profile", profile]
        if config is not None:
            args += ["--config", config]
        if resume:
            args += ["--resume", resume]
        process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=log, text=True,
                                   cwd=self.home, env=dict(os.environ, MYCO_HOME=str(self.home), MYCO_PROFILE="default",
                                                          MYCO_CONFIG='config.toml'))
        self.processes.append(process)
        self.addCleanup(process.stdout.close)
        self.addCleanup(self.stop, process)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            self.assertTrue(selector.select(15), "Server did not print its launch URL")
        launch = process.stdout.readline().strip().removeprefix("Browser UI: ")
        self.assertTrue(launch.startswith("http://127.0.0.1:"), f"Server failed to launch; see {log.name}")
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

    def add_profile(self, name='research'):
        profile = self.home / 'profiles' / name
        (profile / 'workspace/prelude').mkdir(parents=True)
        (profile / 'config.toml').write_text((self.home / 'config.toml').read_text()
                                           .replace('first', name).replace('second', 'spare'))
        return profile

    def profile_pid(self, name):
        for line in subprocess.check_output(['ps', '-eo', 'pid=,ppid=,args='], text=True).splitlines():
            pid, parent, command = line.strip().split(None, 2)
            if int(parent) == self.process.pid and f'--profile {name} --profile-worker ' in command:
                return int(pid)
        return None

    def test_profile_urls_isolate_catalogs_tools_and_identical_session_ids_with_one_browser_stream(self):
        self.add_profile()
        names = [item['name'] for item in self.context.request.get(self.origin + '/api/profiles').json()]
        self.assertEqual(names, ['default', 'research'])
        default = self.session(self.page)
        session_id = default.url.rsplit('/', 1)[1]
        prefix = self.origin + '/profiles/research'
        response = self.context.request.post(prefix + '/api/sessions', data={'request_id': str(uuid.UUID(session_id))})
        self.assertEqual(response.status, 200)
        self.assertEqual(response.json()['id'], session_id)
        other = self.context.new_page()
        renderers = []
        other.on('worker', lambda worker: renderers.append(worker.url))
        other.goto(prefix + '/sessions/' + session_id)
        expect(other.locator('#model')).to_be_enabled()
        expect(other.locator('#sky')).to_have_attribute('data-clouds', 'ready', timeout=30000)
        self.assertIn(prefix + '/cloud-renderer.js', renderers)
        expect(default.locator('#profile-switcher')).to_have_value('default')
        expect(other.locator('#profile-switcher')).to_have_value('research')
        expect(default.locator('#model option')).to_have_text(['first', 'second'])
        expect(other.locator('#model option')).to_have_text(['research', 'spare'])
        self.submit(default, 'Alpha profile')
        expect(default.locator('.assistant .body').last).to_have_text('Alpha finished.')
        expect(other.locator('.user')).to_have_count(0)
        self.submit(other, 'Beta profile')
        expect(other.locator('.assistant .body').last).to_have_text('Beta finished.')
        expect(default.locator('.user .body')).to_have_text('Alpha profile')
        expect(other.locator('.user .body')).to_have_text('Beta profile')
        for label, profile in [('Alpha', 'default'), ('Beta', 'research')]:
            self.assertEqual((self.home / (label + '-profile')).read_text().splitlines(),
                             [profile, str(self.home), str(self.home / 'profiles' / profile / 'workspace'), self.origin + f'/profiles/{profile}'])
            workspace = self.home / 'profiles' / profile / 'workspace'
            self.assertEqual((workspace / 'profile-tool.txt').read_text(), profile)
            url = self.origin + f'/profiles/{profile}/files/profile-tool.txt'
            self.assertEqual(self.context.request.get(url).text(), profile)
        self.assertEqual(self.context.request.get(self.origin + '/files/profile-tool.txt').text(), 'default')
        self.assertFalse((self.home / 'profile-tool.txt').exists())
        self.assertEqual([item['title'] for item in self.context.request.get(self.origin + '/profiles/default/api/sessions').json()], ['Alpha profile'])
        self.assertEqual([item['title'] for item in self.context.request.get(prefix + '/api/sessions').json()], ['Beta profile'])
        targets = self.context.new_cdp_session(default).send('Target.getTargets')['targetInfos']
        self.assertEqual(len([target for target in targets if target['type'] == 'shared_worker' and target['url'].startswith(self.origin)]), 1)
        other.reload()
        expect(other.locator('.assistant .body').last).to_have_text('Beta finished.')
        expect(other).to_have_title('Beta profile · research · myco')
        other.select_option('#profile-switcher', 'default')
        expect(other).to_have_url(self.origin + '/profiles/default/')
        expect(other.locator('.session-name')).to_have_text('Alpha profile')
        other.screenshot(path=str(self.artifacts / 'profiles-desktop.png'))
        other.set_viewport_size({'width': 390, 'height': 844})
        other.screenshot(path=str(self.artifacts / 'profiles-mobile.png'))

    def test_profile_images_and_weather_preferences_stay_in_their_profile(self):
        self.add_profile()
        default = self.session(self.page)
        self.choose_image(default)
        self.submit(default, 'Alpha images')
        expect(default.locator('#model')).to_be_enabled()
        source = default.locator('.user img').get_attribute('src')
        self.assertTrue(source.startswith('/profiles/default/api/image?'), source)
        self.assertEqual(self.context.request.get(self.origin + source).status, 200)
        self.assertEqual(self.context.request.get(self.origin + source.replace('/default/', '/research/')).status, 404)
        other = self.session(profile='research')
        self.choose_image(other)
        self.submit(other, 'Beta images')
        expect(other.locator('#model')).to_be_enabled()
        expect(other.locator('.user img')).to_have_js_property('naturalWidth', 1)
        self.assertTrue(other.locator('.user img').get_attribute('src').startswith('/profiles/research/api/image?'))
        other.reload()
        expect(other.locator('.user img')).to_have_js_property('naturalWidth', 1)
        self.context.route('**/api/sky/weather?*', lambda route: route.fulfill(json={
            'utc_offset_seconds': 0, 'current': {'time': int(time.time()), 'cloud_cover_low': 0,
            'cloud_cover_mid': 65, 'cloud_cover_high': 90, 'wind_speed_10m': 6, 'wind_direction_10m': 250}}))
        default.evaluate("localStorage.setItem('myco.sky.location.v1', JSON.stringify({name: 'Legacy city', latitude: 50, longitude: 0}))")
        default.reload()
        expect(default.locator('#sky')).to_have_attribute('data-weather', 'live')
        expect(other.locator('#sky')).to_have_attribute('data-weather', 'illustrated')
        default.click('#settings-toggle')
        expect(default.locator('#sky-status')).to_contain_text('Legacy city')
        default.click('#sky-reset')
        default.reload()
        expect(default.locator('#sky')).to_have_attribute('data-weather', 'illustrated')
        other.evaluate("localStorage.setItem('myco.sky.location.v1:/profiles/research', JSON.stringify({name: 'Research city', latitude: 51, longitude: 1}))")
        other.reload()
        expect(other.locator('#sky')).to_have_attribute('data-weather', 'live')
        other.click('#settings-toggle')
        expect(other.locator('#sky-status')).to_contain_text('Research city')
        expect(default.locator('#sky')).to_have_attribute('data-weather', 'illustrated')

    def test_selected_profile_owns_launch_overrides_legacy_urls_new_tabs_and_resume(self):
        self.add_profile()
        (self.home / 'profiles/default/config.toml').write_text((self.home / 'config.toml').read_text().replace('first', 'default'))
        self.stop(self.process)
        self.process, launch = self.launch(profile='research')
        self.origin = launch.split('/profiles/')[0]
        self.page.goto(self.origin + '/')
        expect(self.page).to_have_url(launch)
        expect(self.page.locator('#profile-switcher')).to_have_value('research')
        with self.page.expect_popup() as popup:
            self.page.click('#new-session')
        session = popup.value
        session.wait_for_url(re.compile(re.escape(self.origin) + r'/profiles/research/sessions/[a-f0-9]{32}$'))
        expect(session.locator('#model')).to_be_enabled()
        expect(session.locator('#model option')).to_have_text(['first', 'second'])
        default = self.session(profile='default')
        expect(default.locator('#model option')).to_have_text(['default', 'second'])
        session_id = session.url.rsplit('/', 1)[1]
        self.assertEqual([s['id'] for s in self.context.request.get(self.origin + '/api/sessions').json()], [session_id])
        self.page.goto(self.origin + '/sessions/' + session_id)
        expect(self.page).to_have_url(session.url)
        self.stop(self.process)
        self.process, launch = self.launch(resume=session_id, profile='research')
        self.assertTrue(launch.endswith('/profiles/research/sessions/' + session_id), launch)
        self.page.goto(launch)
        expect(self.page.locator('#model')).to_be_enabled()

    def test_profile_workspace_is_created_and_bad_paths_fail_without_affecting_other_profiles(self):
        profile = self.add_profile()
        workspace = profile / 'workspace'
        (workspace / 'prelude').rmdir()
        workspace.rmdir()
        workspace.write_text('A file cannot be used as a working directory.')
        response = self.context.request.get(self.origin + '/profiles/research/api/sessions')
        self.assertEqual(response.status, 503)
        self.assertIn(str(workspace), response.text())
        self.assertEqual(self.context.request.get(self.origin + '/profiles/default/api/sessions').status, 200)
        workspace.unlink()
        other = self.session(profile='research')
        self.assertTrue(workspace.is_dir())
        self.submit(other, 'Beta profile')
        expect(other.locator('.assistant .body').last).to_have_text('Beta finished.')
        self.assertEqual((workspace / 'profile-tool.txt').read_text(), 'research')
        (workspace / 'sibling').symlink_to(self.workspace, target_is_directory=True)
        for path in ['sibling/pixel.png', '..%2Fconfig.toml']:
            self.assertIn(self.context.request.get(self.origin + '/profiles/research/files/' + path).status, [403, 404])
        self.assertEqual(self.context.request.get(self.origin + '/profiles/default/files/config.toml').status, 404)

    def test_relative_environment_config_resolves_before_changing_to_the_profile_workspace(self):
        self.add_profile()
        self.stop(self.process)
        self.process, launch = self.launch(profile='research', config=None)
        self.origin = launch.split('/profiles/')[0]
        page = self.session(self.page, profile='research')
        expect(page.locator('#model option')).to_have_text(['first', 'second'])
        self.submit(page, 'Alpha profile')
        expect(page.locator('.assistant .body').last).to_have_text('Alpha finished.')
        self.assertEqual((self.home / 'profiles/research/workspace/profile-tool.txt').read_text(), 'research')

    def test_profile_failure_recovers_without_interrupting_another_profiles_tool(self):
        research = self.add_profile()
        config = research / 'config.toml'
        valid = config.read_text()
        config.write_text('not a valid configuration')
        other = self.context.new_page()
        response = other.goto(self.origin + '/profiles/research/')
        self.assertEqual(response.status, 503)
        expect(other.get_by_role('heading', name='Profile unavailable')).to_be_visible()
        other.get_by_role('link', name='Choose a profile').click()
        expect(other.get_by_role('link', name='research Open profile')).to_be_visible()
        self.assertEqual(self.context.request.get(self.origin + '/profiles/absent/api/sessions').status, 404)
        self.assertFalse((self.home / 'profiles/absent').exists())
        default = self.session(self.page)
        self.submit(default, 'Alpha wait')
        expect(default.locator('.tool.running')).to_have_count(1)
        config.write_text(valid)
        self.session(other, profile='research')
        self.submit(other, 'Beta images')
        expect(other.locator('#model')).to_be_enabled()
        pid = self.profile_pid('research')
        self.assertIsNotNone(pid)
        os.kill(pid, signal.SIGKILL)
        for _ in range(100):
            replacement = self.profile_pid('research')
            if replacement is not None and replacement != pid:
                break
            other.wait_for_timeout(100)
        self.assertIsNotNone(replacement, 'Profile did not restart from its disconnected tab')
        self.assertNotEqual(replacement, pid)
        expect(other.locator('#model')).to_be_enabled()
        expect(other.locator('.user .body')).to_have_text('Beta images')
        expect(default.locator('.tool.running')).to_have_count(1)
        (self.home / 'Alpha-release').touch()
        expect(default.locator('.assistant .body').last).to_have_text('Alpha finished.')
        self.assertEqual(self.turns['Beta images'], 1, 'Restart must not replay a completed generation')
        self.stop(self.process)
        self.assertIsNone(self.profile_pid('default'))
        self.assertIsNone(self.profile_pid('research'))

    def session(self, page=None, profile=None):
        if page is None:
            page = self.context.new_page()
            page.goto(self.origin)
        prefix = self.origin + (f'/profiles/{profile}' if profile else '')
        response = self.context.request.post(prefix + '/api/sessions', data={'request_id': str(uuid.uuid4())})
        self.assertEqual(response.status, 200)
        page.goto(prefix + '/sessions/' + response.json()['id'])
        expect(page.locator("#model")).to_be_enabled()
        return page

    def submit(self, page, text):
        page.fill("#prompt", text)
        page.press("#prompt", "Enter")
        expect(page.locator(".user").last).to_contain_text(text)

    def image_urls(self, request):
        return [part['image_url'] for item in request['input'] if item.get('role') == 'user'
                for part in item['content'] if isinstance(part, dict) and part.get('type') == 'input_image']

    def test_token_usage_updates_during_tools_across_tabs_and_survives_restart(self):
        page = self.session(self.page)
        expect(page.locator('#context-usage')).to_have_text('Context — / 100K')
        expect(page.locator('#output-tokens')).to_have_text('Output —')
        self.usage = {'input_tokens': 24321, 'output_tokens': 30, 'input_tokens_details': {'cached_tokens': 12000}}
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        expect(page.locator('#context-usage')).to_have_text('Context 24.3K / 100K · 24%')
        expect(page.locator('#context-usage')).to_have_attribute('title', re.compile('24,321 / 100,000 tokens'))
        expect(page.locator('#input-tokens')).to_have_text('Input 24.3K')
        expect(page.locator('#output-tokens')).to_have_text('Output 30')
        expect(page.locator('#cached-tokens')).to_have_text('Cached 12K')
        mirror = self.context.new_page()
        mirror.goto(page.url)
        expect(mirror.locator('#context-usage')).to_have_text('Context 24.3K / 100K · 24%')
        self.usage = {'input_tokens': 32000, 'output_tokens': 8}
        (self.home / 'Alpha-release').touch()
        for tab in [page, mirror]:
            expect(tab.locator('#model')).to_be_enabled()
            expect(tab.locator('#context-usage')).to_have_text('Context 32K / 100K · 32%')
            expect(tab.locator('#output-tokens')).to_have_text('Output 38')
            expect(tab.locator('#cached-tokens')).to_have_text('Cached 0')
        self.submit(page, 'Beta generate')
        expect(page.locator('#cancel')).to_be_visible()
        page.click('#cancel')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#output-tokens')).to_have_text('Output 38')
        self.submit(page, 'Alpha fail')
        expect(page.locator('#connection')).to_have_text('Stopped')
        expect(page.locator('#output-tokens')).to_have_text('Output 38')
        session_url = page.url
        page.reload()
        expect(page.locator('#context-usage')).to_have_text('Context 32K / 100K · 32%')
        self.stop(self.process)
        self.process, _ = self.launch(port=urlsplit(self.origin).port)
        page.goto(session_url)
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#output-tokens')).to_have_text('Output 38')
        expect(page.locator('#context-usage')).to_have_text('Context 32K / 100K · 32%')
        self.usage = {'input_tokens': 1500, 'output_tokens': 7}
        self.submit(page, 'Alpha wait')
        expect(page.locator('#output-tokens')).to_have_text('Output 7')

    def test_model_change_updates_context_capacity_and_distinguishes_missing_usage_from_zero(self):
        self.stop(self.process)
        config = self.home / 'config.toml'
        first, second = config.read_text().split('[models.second]')
        config.write_text(first + '[models.second]' + second.replace('context_window = 100000', 'context_window = 200000'))
        self.process, _ = self.launch(port=urlsplit(self.origin).port)
        page = self.session(self.page)
        self.submit(page, 'Alpha images')
        expect(page.locator('#output-tokens')).to_have_text('Output 20')
        expect(page.locator('#model')).to_be_enabled()
        page.select_option('#model', 'second')
        expect(page.locator('#model')).to_have_value('second')
        expect(page.locator('#context-usage')).to_have_text('Context — / 200K')
        expect(page.locator('#output-tokens')).to_have_text('Output —')
        self.usage = None
        self.submit(page, 'Beta images')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#context-usage')).to_have_text('Context — / 200K')
        self.usage = {'input_tokens': 0, 'output_tokens': 0}
        self.submit(page, 'Beta images')
        expect(page.locator('#context-usage')).to_have_text('Context 0 / 200K · 0%')
        expect(page.locator('#input-tokens')).to_have_text('Input 0')
        expect(page.locator('#output-tokens')).to_have_text('Output 0')

    def test_auto_compaction_defaults_to_the_full_context_window(self):
        self.compaction_release.clear()
        self.usage = {'input_tokens': 100000, 'output_tokens': 20}
        self.turns['Alpha images'] = 1
        page = self.session(self.page)
        self.submit(page, 'Alpha images')
        expect(page.locator('#connection')).to_have_text('Compacting')
        expect(page.locator('#composer-frame')).to_have_attribute('data-state', 'running')
        expect(page.locator('.assistant .body')).to_have_text('Alpha finished.')
        self.usage = {'input_tokens': 512, 'output_tokens': 6}
        self.compaction_release.set()
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.assistant .body')).to_have_text(['Alpha finished.'] * 2)
        expect(page.locator('#context-usage')).to_have_text('Context 512 / 100K · 1%')
        expect(page.locator('#composer-frame')).to_have_attribute('data-state', 'ready')
        expect(page.locator('#transcript')).not_to_contain_text('Resumption')

    def test_smaller_model_compacts_before_the_next_message_even_after_restart(self):
        self.stop(self.process)
        config = self.home / 'config.toml'
        first, second = config.read_text().split('[models.second]')
        config.write_text(first + '[models.second]' + second.replace('context_window = 100000', 'context_window = 50000'))
        self.process, _ = self.launch(port=urlsplit(self.origin).port)
        self.usage = {'input_tokens': 60000, 'output_tokens': 20}
        self.turns.update({'Alpha images': 1, 'Beta images': 1})
        page = self.session(self.page)
        self.submit(page, 'Alpha images')
        expect(page.locator('#model')).to_be_enabled()
        page.select_option('#model', 'second')
        expect(page.locator('#model')).to_have_value('second')
        expect(page.locator('#context-usage')).to_have_text('Context — / 50K')
        self.assertEqual(len(self.requests), 1, 'Selecting a model must wait for a message')
        self.stop(self.process)
        self.process, _ = self.launch(port=urlsplit(self.origin).port)
        page.reload()
        expect(page.locator('#model')).to_have_value('second')
        expect(page.locator('#model')).to_be_enabled()
        self.assertEqual(len(self.requests), 1, 'Opening a session must not compact or continue')
        self.compaction_release.clear()
        self.submit(page, 'Beta images')
        expect(page.locator('#connection')).to_have_text('Compacting')
        self.assertEqual(self.turns['Beta images'], 1,
                         'The new model must not receive the uncompressed task first')
        self.usage = {'input_tokens': 512, 'output_tokens': 6}
        self.compaction_release.set()
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.user .body')).to_have_text(['Alpha images', 'Beta images'])
        expect(page.locator('.assistant .body').last).to_have_text('Beta finished.')
        expect(page.locator('#context-usage')).to_have_text('Context 512 / 50K · 1%')

    def test_compaction_reports_activity_without_internal_messages_and_continues_afterward(self):
        for automatic in [False, True]:
            with self.subTest(automatic=automatic):
                if automatic:
                    self.stop(self.process)
                    config = self.home / 'config.toml'
                    config.write_text(config.read_text().replace('context_window = 100000',
                                      'context_window = 100000\nauto_compact_at = 0.8'))
                    self.process, _ = self.launch(port=urlsplit(self.origin).port)
                self.compaction_release.clear()
                self.continuation_release.clear()
                self.usage = {'input_tokens': 80000, 'output_tokens': 20}
                self.turns['Alpha images'] = 1
                page = self.session(self.page)
                self.submit(page, 'Alpha images')
                if not automatic:
                    expect(page.locator('#model')).to_be_enabled()
                    page.click('#compact')
                expect(page.locator('#connection')).to_have_text('Compacting')
                expect(page.locator('.compaction')).to_have_count(0)
                expect(page.locator('.user .body')).to_have_text('Alpha images')
                expect(page.locator('.assistant .body')).to_have_text('Alpha finished.')
                page.reload()
                expect(page.locator('#connection')).to_have_text('Compacting')
                expect(page.locator('.compaction')).to_have_count(0)
                if automatic:
                    page.set_viewport_size({'width': 390, 'height': 844})
                page.screenshot(path=str(self.artifacts / ('automatic-mobile.png' if automatic else 'manual-desktop.png')))
                self.usage = {'input_tokens': 512, 'output_tokens': 6}
                self.compaction_release.set()
                if automatic:
                    expect(page.locator('#connection')).to_have_text('Running')
                    expect(page.locator('.assistant')).to_have_count(1)
                self.continuation_release.set()
                expect(page.locator('#model')).to_be_enabled()
                expect(page.locator('.assistant .body')).to_have_text(['Alpha finished.'] * (2 if automatic else 1))
                expect(page.locator('#transcript')).not_to_contain_text(re.compile('Summary saved|Continue the browser fixture task|Resumption|Prelude changes|SYSTEM'))
                page.screenshot(path=str(self.artifacts / ('continued-mobile.png' if automatic else 'completed-desktop.png')))
                requests = len(self.requests)
                self.stop(self.process)
                self.process, _ = self.launch(port=urlsplit(self.origin).port)
                page.reload()
                expect(page.locator('#model')).to_be_enabled()
                expect(page.locator('.compaction')).to_have_count(0)
                expect(page.locator('.user .body')).to_have_text('Alpha images')
                expect(page.locator('.assistant .body')).to_have_text(['Alpha finished.'] * (2 if automatic else 1))
                expect(page.locator('#transcript')).not_to_contain_text(re.compile('Summary saved|Continue the browser fixture task|Resumption|Prelude changes|SYSTEM'))
                self.assertEqual(len(self.requests), requests, 'Reloading must not generate another continuation')

    def test_payload_rejection_recovers_after_tools_without_resending_images(self):
        self.reject_payload = lambda body: (any(item.get('type') == 'function_call_output'
                                               for item in body['input'])
                                            and '"type": "input_image"' in json.dumps(body))
        self.compaction_release.clear()
        page = self.session(self.page)
        state_url = page.url.replace('/sessions/', '/api/sessions/')
        original_thread = self.context.request.get(state_url).json()['change']['snapshot']['thread_id']
        self.choose_image(page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        (self.home / 'Alpha-release').touch()
        expect(page.locator('#connection')).to_have_text('Compacting')
        expect(page.locator('.tool.running')).to_have_count(0)
        self.compaction_release.set()
        expect(page.locator('.assistant .body').last).to_have_text('Alpha finished.')
        expect(page.locator('#model')).to_be_enabled()
        self.assertEqual((self.home / 'Alpha-done').read_text(), 'done')
        recovered = self.context.request.get(state_url).json()['change']['snapshot']
        self.assertNotEqual(recovered['thread_id'], original_thread)
        self.assertNotIn('"type": "input_image"', json.dumps(self.requests[-1]))
        for refreshed in [False, True]:
            if refreshed:
                page.reload()
            expect(page.locator('.user .body')).to_contain_text('Alpha wait')
            expect(page.locator('.user .body')).to_contain_text('Image omitted to reduce request size')
            expect(page.locator('.user img')).to_have_count(0)
            expect(page.locator('.tool')).to_have_count(1)
            expect(page.locator('.assistant .body').last).to_have_text('Alpha finished.')

    def test_local_request_size_cap_compacts_before_upload_and_continues(self):
        self.stop(self.process)
        config = self.home / 'config.toml'
        config.write_text(config.read_text().replace('[models.first]',
                          '[models.first]\nmax_request_bytes = 100000'))
        self.process, _ = self.launch(port=urlsplit(self.origin).port)
        self.compaction_release.clear()
        self.turns['Alpha images'] = 1
        page = self.session(self.page)
        self.choose_image(page, buffer=(self.home / 'pixel.png').read_bytes() + bytes(150000))
        self.submit(page, 'Alpha images')
        expect(page.locator('#connection')).to_have_text('Compacting')
        self.compaction_release.set()
        expect(page.locator('.assistant .body').last).to_have_text('Alpha finished.')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.user .body')).to_contain_text('Image omitted to reduce request size')
        self.assertTrue(self.requests)
        self.assertTrue(all(not self.image_urls(request) for request in self.requests),
                        'The oversized image request must fail locally before upload')

    def test_print_mode_recovers_from_http_413_and_exits_successfully(self):
        self.reject_payload = lambda _body: self.turns.get('Alpha markdown') == 1
        result = subprocess.run([str(OPTIONS.binary), '--config', 'config.toml', '-p', 'Alpha markdown'],
                                cwd=self.home, env=dict(os.environ, MYCO_HOME=str(self.home),
                                                       MYCO_PROFILE='default', MYCO_CONFIG='config.toml'),
                                capture_output=True, text=True, timeout=20)
        (self.artifacts / 'cli.stdout').write_text(result.stdout)
        (self.artifacts / 'cli.stderr').write_text(result.stderr)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), 'Alpha finished.')
        self.assertIn('compaction complete', result.stderr)
        self.assertEqual(self.turns['Alpha markdown'], 2)

    def test_cancelled_compaction_keeps_the_source_and_delivers_queued_input_without_a_success_card(self):
        self.turns.update({'Alpha images': 1, 'Beta images': 1})
        page = self.session(self.page)
        self.submit(page, 'Alpha images')
        expect(page.locator('#model')).to_be_enabled()
        state_url = self.origin + '/api/sessions/' + page.url.rsplit('/', 1)[1]
        thread = self.context.request.get(state_url).json()['change']['snapshot']['thread_id']
        self.compaction_release.clear()
        page.click('#compact')
        expect(page.locator('#connection')).to_have_text('Compacting')
        page.fill('#prompt', 'Beta images')
        page.press('#prompt', 'Enter')
        expect(page.locator('#queued-list .queued-content')).to_have_text(['Beta images'])
        page.click('#cancel')
        expect(page.locator('.assistant .body').last).to_have_text('Beta finished.')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.compaction')).to_have_count(0)
        expect(page.locator('#queued-list .queued-content')).to_have_count(0)
        self.assertEqual(self.context.request.get(state_url).json()['change']['snapshot']['thread_id'], thread)
        page.reload()
        expect(page.locator('.compaction')).to_have_count(0)
        expect(page.locator('.assistant .body').last).to_have_text('Beta finished.')

    def test_prelude_changes_reach_the_model_but_stay_out_of_the_transcript(self):
        profile = self.add_profile()
        page = self.session(self.page, profile='research')
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        (profile / 'workspace/prelude/live-change.md').write_text('Use the current project conventions.')
        (self.home / 'Alpha-release').touch()
        expect(page.locator('.assistant .body').last).to_have_text('Alpha finished.')
        expect(page.locator('#model')).to_be_enabled()
        self.assertIn('[myco: Prelude changes]', json.dumps(self.requests[-1]))
        self.assertIn('live-change.md', json.dumps(self.requests[-1]))
        for restarted in [False, True]:
            if restarted:
                self.stop(self.process)
                self.process, _ = self.launch(port=urlsplit(self.origin).port)
            page.reload()
            expect(page.locator('#model')).to_be_enabled()
            expect(page.locator('.user .body')).to_have_text('Alpha wait')
            expect(page.locator('.assistant .body').last).to_have_text('Alpha finished.')
            expect(page.locator('#transcript')).not_to_contain_text('Prelude changes')
            expect(page.locator('#transcript')).not_to_contain_text('live-change.md')

    def test_compaction_clears_context_measurement_until_the_next_model_request(self):
        page = self.session(self.page)
        self.usage = {'input_tokens': 80000, 'output_tokens': 20}
        self.submit(page, 'Alpha images')
        expect(page.locator('#context-usage')).to_have_text('Context 80K / 100K · 80%')
        expect(page.locator('#model')).to_be_enabled()
        page.click('#compact')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#context-usage')).to_have_text('Context — / 100K')
        expect(page.locator('#output-tokens')).to_have_text('Output —')
        page.reload()
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#context-usage')).to_have_text('Context — / 100K')
        self.usage = {'input_tokens': 512, 'output_tokens': 6}
        self.submit(page, 'Beta images')
        expect(page.locator('#context-usage')).to_have_text('Context 512 / 100K · 1%')
        expect(page.locator('#output-tokens')).to_have_text('Output 6')

    def test_message_timestamps_use_local_time_and_update_without_a_new_turn(self):
        context = self.browser.new_context(locale='en-US', timezone_id='America/Los_Angeles')
        self.addCleanup(context.close)
        page = self.session(context.new_page())
        page.on('pageerror', lambda error: self.errors.append(str(error)))
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        timestamps = page.locator('.message-header time')
        expect(timestamps).to_have_count(2)
        stamp = timestamps.first.get_attribute('datetime')
        instant = datetime.fromisoformat(stamp.replace('Z', '+00:00'))
        local = instant.astimezone(ZoneInfo('America/Los_Angeles'))
        label = f'{local:%b} {local.day}, {local.year}, {local.hour % 12 or 12}:{local:%M:%S %p}'
        page.clock.set_fixed_time(instant + timedelta(seconds=59))
        expect(timestamps).to_have_text([f'{label} (0 minutes ago)'] * 2)
        page.clock.set_fixed_time(instant + timedelta(minutes=1))
        expect(timestamps).to_have_text([f'{label} (1 minute ago)'] * 2)
        page.clock.set_fixed_time(instant + timedelta(minutes=3))
        expect(timestamps).to_have_text([f'{label} (3 minutes ago)'] * 2)
        page.reload()
        expect(timestamps).to_have_text([f'{label} (3 minutes ago)'] * 2)
        expect(timestamps.first).to_have_attribute('datetime', stamp)
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: true})")
        page.clock.set_fixed_time(instant + timedelta(minutes=7))
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: false}); document.dispatchEvent(new Event('visibilitychange'))")
        expect(timestamps).to_have_text([f'{label} (7 minutes ago)'] * 2)
        page.click('#cancel')
        expect(page.locator('#model')).to_be_enabled()

    def test_timestamp_timezones_daylight_saving_and_missing_dates(self):
        for zone, spring, fall in [
            ('America/Los_Angeles', 'Mar 8, 2026, 1:58:00 AM', 'Nov 1, 2026, 1:58:00 AM'),
            ('Asia/Tokyo', 'Mar 8, 2026, 6:58:00 PM', 'Nov 1, 2026, 5:58:00 PM'),
        ]:
            context = self.browser.new_context(locale='en-US', timezone_id=zone)
            self.addCleanup(context.close)
            page = context.new_page()
            page.on('pageerror', lambda error: self.errors.append(str(error)))
            page.clock.set_fixed_time('2026-03-08T10:02:00Z')
            page.goto(self.origin)
            page.evaluate("""async () => {
                const {messageTimestamp} = await import('/timestamps.js');
                const box = document.createElement('section'); box.id = 'timestamp-test';
                box.append(...['2026-03-08T09:58:00Z', '2026-11-01T08:58:00Z', null, 'invalid',
                    '2026-03-08T10:03:00Z'].map(messageTimestamp));
                document.body.append(box);
            }""")
            timestamps = page.locator('#timestamp-test time')
            expect(timestamps.nth(0)).to_have_text(spring + ' (4 minutes ago)')
            expect(timestamps.nth(2)).to_have_text('unknown')
            expect(timestamps.nth(3)).to_have_text('unknown')
            self.assertIsNone(timestamps.nth(2).get_attribute('datetime'))
            expect(timestamps.nth(4)).to_contain_text('(0 minutes ago)')
            self.assertTrue(timestamps.nth(0).get_attribute('title'))
            page.clock.set_fixed_time('2026-11-01T09:02:00Z')
            expect(timestamps.nth(1)).to_have_text(fall + ' (4 minutes ago)')
            context.close()

    def test_urls_are_clickable_in_messages_and_tool_output_after_reload(self):
        page = self.session(self.page)
        (self.workspace / 'linked.txt').write_text('Opened the link target.')
        url = self.origin + '/files/linked.txt?from=review&mode=full#details'
        self.link_reply = (f'Review {url}.\n\n[Named preview]({url})\n\n`{url}`\n\n```text\n{url}\n```\n\n'
                           'Also https://example.com/a_(b), www.example.com/docs.')
        prompt = f'Alpha links: {url}.\n<b>literal user text</b>'
        self.submit(page, prompt)
        expect(page.locator('#model')).to_be_enabled()
        for reload in [False, True]:
            if reload:
                page.reload()
            expect(page.locator('.user .body')).to_have_text(prompt)
            expect(page.locator('.user .body b')).to_have_count(0)
            expect(page.locator('.user .body a')).to_have_attribute('href', url)
            expect(page.locator('.assistant .body a')).to_have_count(4)
            expect(page.locator('.assistant .body a').nth(2)).to_have_attribute('href', 'https://example.com/a_(b)')
            expect(page.locator('.assistant .body a').nth(3)).to_have_attribute('href', 'https://www.example.com/docs')
            expect(page.locator('.assistant code a')).to_have_count(0)
            expect(page.locator('.assistant a a')).to_have_count(0)
            expect(page.locator('.tool .output a')).to_have_attribute('href', self.origin + '/files/linked.txt?from=tool&mode=full#details')
            for link in page.locator('.user .body a, .assistant .body a, .tool .output a').all():
                expect(link).to_have_attribute('target', '_blank')
                expect(link).to_have_attribute('rel', 'noopener noreferrer')
        with self.context.expect_page() as opened:
            page.locator('.user .body a').click()
        expect(opened.value.locator('body')).to_have_text('Opened the link target.')
        self.assertTrue(page.url.startswith(self.origin + '/profiles/default/sessions/'))

    def test_url_boundaries_punctuation_and_queued_links_preserve_text(self):
        page = self.session(self.page)
        text = ('See (https://example.com/a_(b)), www.example.com/docs.\n'
                'HTTP://localhost:3000/path?q=one&other=two#section!\n'
                '[http://[::1]:8080/test] and https://example.com/雪.\n'
                'Skip https:// and javascript:alert(1), me@www.example.com, /www.example.com, prefixhttps://example.com.')
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        page.fill('#prompt', text)
        page.click('#send')
        queued = page.locator('#queued-list .queued-content')
        expect(queued).to_have_text(text)
        expected = ['https://example.com/a_(b)', 'https://www.example.com/docs',
                    'http://localhost:3000/path?q=one&other=two#section',
                    'http://[::1]:8080/test', 'https://example.com/%E9%9B%AA']
        self.assertEqual(queued.locator('a').evaluate_all('nodes => nodes.map(n => n.href)'), expected)
        page.reload()
        expect(queued.locator('a')).to_have_count(5)
        (self.home / 'Alpha-release').touch()
        expect(page.locator('#model')).to_be_enabled()
        user = page.locator('.user .body').last
        expect(user).to_have_text(text)
        self.assertEqual(user.locator('a').evaluate_all('nodes => nodes.map(n => n.href)'), expected)

    def choose_image(self, page, name='local image.png', buffer=None):
        page.set_input_files('#attachment-picker', {'name': name, 'mimeType': 'image/png',
            'buffer': buffer if buffer is not None else (self.home / 'pixel.png').read_bytes()})
        expect(page.locator('#attachment-list img').last).to_have_js_property('naturalWidth', 1)

    def test_attached_images_can_be_removed_and_survive_reload_restart_and_resume(self):
        page = self.session(self.page)
        with page.expect_file_chooser() as chooser:
            page.click('#attach')
        chooser.value.set_files(self.home / 'pixel.png')
        self.choose_image(page)
        expect(page.locator('#attachment-list img')).to_have_count(2)
        page.get_by_role('button', name='Remove pixel.png', exact=True).click()
        expect(page.locator('#attachment-list img')).to_have_count(1)
        self.submit(page, 'Alpha images')
        expect(page.locator('#attachments')).to_be_hidden()
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.user img')).to_have_js_property('naturalWidth', 1)
        images = self.image_urls(self.requests[-1])
        self.assertEqual(len(images), 1)
        self.assertEqual(base64.b64decode(images[0].split(',')[1]), (self.home / 'pixel.png').read_bytes())
        session_id = page.url.rsplit('/', 1)[1]
        saved = self.home / f'profiles/default/session/{session_id[:2]}/{session_id}.json'
        self.assertIn('myco-image:sha256:', saved.read_text())
        self.assertNotIn('data:image/', saved.read_text())
        page.reload()
        expect(page.locator('.user img')).to_have_js_property('naturalWidth', 1)
        self.stop(self.process)
        self.process, _ = self.launch(port=urlsplit(self.origin).port)
        expect(page.locator('#model')).to_be_enabled(timeout=15000)
        expect(page.locator('.user img')).to_have_js_property('naturalWidth', 1)
        self.submit(page, 'Beta images')
        expect(page.locator('#model')).to_be_enabled()
        self.assertEqual(self.image_urls(self.requests[-1]), images)

    def test_native_clipboard_paste_sends_an_image_only_message_and_keeps_text_paste(self):
        page = self.session(self.page)
        self.context.grant_permissions(['clipboard-read', 'clipboard-write'])
        page.evaluate("""async () => {
            const canvas = document.createElement('canvas');
            canvas.width = canvas.height = 1;
            canvas.getContext('2d').fillRect(0, 0, 1, 1);
            const blob = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
            await navigator.clipboard.write([new ClipboardItem({'image/png': blob})]);
        }""")
        page.focus('#prompt')
        page.keyboard.press('Control+V')
        expect(page.locator('#attachment-list img')).to_have_js_property('naturalWidth', 1)
        expect(page.locator('#prompt')).to_have_value('')
        page.click('#send')
        expect(page.locator('.user img')).to_have_js_property('naturalWidth', 1)
        expect(page.locator('#model')).to_be_enabled()
        self.assertEqual(len(self.image_urls(self.requests[-1])), 1)
        for item in self.requests[-1]['input']:
            if item.get('role') == 'user':
                self.assertTrue(all(part.get('text', 'image').strip() for part in item['content']))
        page.evaluate("navigator.clipboard.writeText('plain pasted text')")
        page.focus('#prompt')
        page.keyboard.press('Control+V')
        expect(page.locator('#prompt')).to_have_value('plain pasted text')
        expect(page.locator('#attachments')).to_be_hidden()

    def test_dropped_images_queue_with_tool_results_and_survive_a_tab_reload(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        pixel = base64.b64encode((self.home / 'pixel.png').read_bytes()).decode()
        transfer = page.evaluate_handle("""data => {
            const transfer = new DataTransfer();
            transfer.items.add(new File([Uint8Array.from(atob(data), c => c.charCodeAt(0))], 'dropped.png', {type: 'image/png'}));
            return transfer;
        }""", pixel)
        page.dispatch_event('#composer', 'drop', {'dataTransfer': transfer})
        expect(page.locator('#attachment-list img')).to_have_js_property('naturalWidth', 1)
        page.fill('#prompt', 'Beta images')
        page.click('#send')
        expect(page.locator('#queued-list img')).to_have_js_property('naturalWidth', 1)
        expect(page.locator('#attachments')).to_be_hidden()
        page.reload()
        expect(page.locator('#queued-list img')).to_have_js_property('naturalWidth', 1)
        (self.home / 'Alpha-release').touch()
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.user img')).to_have_js_property('naturalWidth', 1)
        expect(page.locator('#queued')).to_be_hidden()
        request = self.requests[-1]['input']
        result = next(i for i, item in enumerate(request) if item.get('type') == 'function_call_output')
        image = next(i for i, item in enumerate(request) if 'input_image' in json.dumps(item))
        self.assertGreater(image, result)

    def test_cancel_sends_queued_image_only_messages(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        self.choose_image(page)
        page.click('#send')
        expect(page.locator('#queued-list img')).to_have_js_property('naturalWidth', 1)
        page.click('#cancel')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#queued')).to_be_hidden()
        expect(page.locator('.user img')).to_have_js_property('naturalWidth', 1)
        self.assertEqual(len(self.image_urls(self.requests[-1])), 1)
        self.assertIn('cancel', json.dumps(self.requests[-1]['input']).lower())

    def test_lost_image_submission_response_preserves_draft_and_deduplicates_retry(self):
        page = self.session(self.page)
        attempts = []
        def lose_once(route):
            attempts.append(route.request.post_data_json['request_id'])
            response = route.fetch()
            if len(attempts) == 1:
                route.abort()
            else:
                route.fulfill(response=response)
        page.route('**/api/sessions/*/action', lose_once)
        self.choose_image(page)
        self.submit(page, 'Alpha images')
        expect(page.locator('#error')).to_contain_text('Your draft is still here')
        expect(page.locator('#prompt')).to_have_value('Alpha images')
        expect(page.locator('#attachment-list img')).to_have_count(1)
        expect(page.locator('#model')).to_be_enabled()
        page.click('#send')
        expect(page.locator('#attachments')).to_be_hidden()
        expect(page.locator('#prompt')).to_have_value('')
        self.assertEqual(attempts[0], attempts[1])
        self.assertEqual(len(self.requests), 1)
        expect(page.locator('.user')).to_have_count(1)

    def test_image_actions_accept_payloads_larger_than_other_json_routes(self):
        page = self.session(self.page)
        image = (self.home / 'pixel.png').read_bytes() + bytes(1600 * 1024)
        self.choose_image(page, buffer=image)
        self.submit(page, 'Alpha images')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#attachments')).to_be_hidden()
        self.assertEqual(base64.b64decode(self.image_urls(self.requests[-1])[0].split(',')[1]), image)
        response = self.context.request.post(self.origin + '/api/markdown', data={'text': 'x' * (2 * 1024 * 1024)})
        self.assertEqual(response.status, 413)

    def test_invalid_image_actions_keep_the_session_idle(self):
        page = self.session(self.page)
        session_id = page.url.rsplit('/', 1)[1]
        pixel = 'data:image/png;base64,' + base64.b64encode((self.home / 'pixel.png').read_bytes()).decode()
        for images in [['data:image/png;base64,!'], ['data:image/png;base64,dGV4dA=='], [pixel] * 21, ['file:///tmp/pixel.png']]:
            response = self.context.request.post(page.url.replace('/sessions/', '/api/sessions/') + '/action',
                data={'request_id': str(uuid.uuid4()), 'session_id': session_id,
                      'action': {'kind': 'submit', 'text': 'Alpha images', 'images': images}})
            self.assertEqual(response.status, 400, response.text())
        expect(page.locator('#connection')).to_have_text('Ready')
        self.assertEqual(self.requests, [])
        self.assertFalse(self.context.request.get(self.origin + '/api/sessions/' + session_id).json()['change']['snapshot']['busy'])

    def test_model_image_limit_is_rechecked_without_losing_the_draft(self):
        self.stop(self.process)
        config = self.home / 'config.toml'
        config.write_text(config.read_text().replace('[models.second]', '[models.second]\nmax_image_base64_bytes = 1048576'))
        self.process, _ = self.launch(port=urlsplit(self.origin).port)
        page = self.session(self.page)
        self.choose_image(page, buffer=(self.home / 'pixel.png').read_bytes() + bytes(1600 * 1024))
        page.fill('#prompt', 'Alpha images')
        page.select_option('#model', 'second')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#model')).to_have_value('second')
        page.click('#send')
        expect(page.locator('#error')).to_contain_text("the model's limit is 1.0 MiB")
        expect(page.locator('#prompt')).to_have_value('Alpha images')
        expect(page.locator('#attachment-list img')).to_have_count(1)
        self.assertEqual(self.requests, [])
        page.select_option('#model', 'first')
        expect(page.locator('#model')).to_be_enabled()
        page.click('#send')
        expect(page.locator('#attachments')).to_be_hidden()
        expect(page.locator('#model')).to_be_enabled()
        self.assertEqual(len(self.image_urls(self.requests[-1])), 1)

    def test_uploaded_and_server_path_images_share_the_message_budget(self):
        page = self.session(self.page)
        paths = [self.home / f'large-{index}.png' for index in range(4)]
        pixel = (self.home / 'pixel.png').read_bytes()
        for path in paths:
            path.write_bytes(pixel + bytes(3 * 1024 * 1024 + 700 * 1024))
        image = 'data:image/png;base64,' + base64.b64encode(pixel + bytes(512 * 1024)).decode()
        response = self.context.request.post(page.url.replace('/sessions/', '/api/sessions/') + '/action',
            data={'request_id': str(uuid.uuid4()), 'session_id': page.url.rsplit('/', 1)[1],
                  'action': {'kind': 'submit', 'text': ' '.join(f'@{path}' for path in paths), 'images': [image]}})
        self.assertEqual(response.status, 400, response.text())
        self.assertIn('per-message limit of 20.0 MiB', response.text())
        expect(page.locator('#connection')).to_have_text('Ready')
        self.assertEqual(self.requests, [])

    def browser_status(self, browser, session):
        path = urlsplit(session.url).path
        return browser.locator(f'#session-list a[href="{path}"] .session-status')

    def pause_browser_polling(self, browser):
        expect(browser.locator('#connection')).to_have_text('Live')
        browser.clock.install()
        browser.clock.pause_at(browser.evaluate('Date.now() / 1000 + 1'))

    def test_activity_is_live_before_first_output_and_isolated_between_sessions(self):
        home = self.page
        alpha, beta = self.session(), self.session()
        alpha_status, beta_status = self.browser_status(home, alpha), self.browser_status(home, beta)
        expect(alpha_status).to_have_text('Ready')
        expect(beta_status).to_have_text('Ready')
        self.pause_browser_polling(home)
        for page, status, name in [(alpha, alpha_status, 'Alpha'), (beta, beta_status, 'Beta')]:
            self.submit(page, f'{name} generate')
            expect(page.locator('#connection')).to_have_text('Running')
            expect(page.locator('#connection')).to_have_attribute('data-busy', 'true')
            expect(status).to_have_text('Running', timeout=2000)
            expect(status).to_have_attribute('data-busy', 'true')
            expect(page.locator('.assistant, .tool')).to_have_count(0)
        self.assertEqual(alpha.locator('#connection').evaluate("n => getComputedStyle(n, '::before').animationName"), 'activity-pulse')
        alpha.emulate_media(reduced_motion='reduce')
        self.assertEqual(alpha.locator('#connection').evaluate("n => getComputedStyle(n, '::before').animationName"), 'none')
        alpha.emulate_media(reduced_motion='no-preference')
        alpha.screenshot(path=str(self.artifacts / 'session-running.png'))
        home.screenshot(path=str(self.artifacts / 'sessions-running.png'))
        self.generation_releases['Alpha'].set()
        expect(alpha.locator('#connection')).to_have_text('Ready')
        expect(alpha_status).to_have_text('Ready', timeout=2000)
        expect(alpha_status).to_have_attribute('data-busy', 'false')
        expect(beta_status).to_have_text('Running')
        beta.click('#cancel')
        expect(beta.locator('#connection')).to_have_attribute('data-busy', 'false')
        expect(beta_status).to_have_attribute('data-busy', 'false', timeout=2000)

    def test_composer_light_follows_running_stopped_ready_and_connection_state(self):
        page = self.session(self.page)
        frame, composer = page.locator('#composer-frame'), page.locator('#composer')
        light = page.locator('.composer-light')
        expect(frame).to_have_attribute('data-state', 'ready')
        idle_border = composer.evaluate('n => getComputedStyle(n).borderTopColor')
        self.submit(page, 'Alpha generate')
        expect(page.locator('#connection')).to_have_text('Running')
        expect(frame).to_have_attribute('data-state', 'running')
        expect(composer).not_to_have_css('border-top-color', idle_border)
        page.wait_for_function("document.querySelector('#composer-frame').getAnimations({subtree: true}).some(a => a.playState === 'running' && a.effect.getComputedTiming().iterations === Infinity)")
        before = light.evaluate("n => getComputedStyle(n, '::before').backgroundImage")
        page.wait_for_function("before => getComputedStyle(document.querySelector('.composer-light'), '::before').backgroundImage !== before", arg=before)
        self.assertIn('drop-shadow', light.evaluate('n => getComputedStyle(n).filter'))
        page.emulate_media(reduced_motion='reduce')
        self.assertFalse(frame.evaluate('n => n.getAnimations({subtree: true}).some(a => a.effect.getComputedTiming().iterations === Infinity)'))
        expect(light).to_have_css('opacity', '1')
        page.emulate_media(reduced_motion='no-preference')
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: true}); document.dispatchEvent(new Event('visibilitychange'))")
        self.assertEqual(light.evaluate("n => getComputedStyle(n, '::before').animationPlayState"), 'paused')
        page.evaluate("Object.defineProperty(document, 'hidden', {configurable: true, value: false}); document.dispatchEvent(new Event('visibilitychange'))")
        page.click('#cancel')
        expect(frame).to_have_attribute('data-state', 'stopped')
        stopped_border = composer.evaluate('n => getComputedStyle(n).borderTopColor')
        channels = [float(value) for value in re.findall(r'[\d.]+', stopped_border)][:3]
        self.assertGreater(channels[0], channels[1])
        self.assertGreater(channels[0], channels[2])
        page.reload()
        expect(frame).to_have_attribute('data-state', 'stopped')
        expect(composer).to_have_css('border-top-color', stopped_border)
        self.turns['Beta images'] = 1
        self.submit(page, 'Beta images')
        expect(frame).to_have_attribute('data-state', 'ready')
        expect(composer).to_have_css('border-top-color', idle_border)
        self.stop(self.process)
        expect(frame).to_have_attribute('data-state', 'reconnecting')
        expect(composer).not_to_have_css('border-top-color', idle_border)
        self.process, _ = self.launch(port=urlsplit(self.origin).port)
        expect(frame).to_have_attribute('data-state', 'ready', timeout=15000)

    def test_background_button_keeps_process_alive_across_reload_and_cancellation_of_a_later_turn(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        control = page.locator('.tool.running .background-tool')
        expect(control).to_be_visible()
        page.reload()
        expect(control).to_be_visible()
        control.click()
        expect(page.locator('.tool-status')).to_have_text('running')
        expect(page.locator('.assistant .body').last).to_have_text('Alpha finished.')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#connection')).to_have_text('Running')
        expect(page.locator('#composer-frame')).to_have_attribute('data-state', 'running')
        results = [item for item in self.requests[-1]['input'] if item.get('type') == 'function_call_output']
        self.assertIn('User backgrounded this call', json.dumps(results))
        self.assertFalse((self.home / 'Alpha-done').exists())
        page.click('#activity-toggle')
        expect(page.locator('#activity-list .active-call')).to_have_count(1)
        page.locator('#activity-list .active-call').click()
        expect(page.locator('#activity')).not_to_be_visible()
        expect(page.locator('.tool')).to_have_attribute('open', '')
        expect(page.locator('.tool .output')).to_contain_text('session_id: bg-')
        timer = page.locator('.tool .tool-duration').text_content()
        page.wait_for_function("before => document.querySelector('.tool .tool-duration').textContent !== before", arg=timer)
        page.reload()
        expect(page.locator('.tool.running')).to_have_count(1)
        expect(page.locator('.tool .background-tool')).to_have_count(0)
        self.submit(page, 'Alpha generate')
        expect(page.locator('#connection')).to_have_text('Running')
        page.click('#cancel')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#composer-frame')).to_have_attribute('data-state', 'stopped')
        (self.home / 'Alpha-release').touch()
        expect(page.locator('#connection')).to_have_text('Stopped')
        self.assertEqual((self.home / 'Alpha-done').read_text(), 'done')
        page.reload()
        expect(page.locator('.tool-status')).to_have_text('exit 0')
        expect(page.locator('.background-tool')).to_have_count(0)

    def test_background_controls_isolate_sessions_and_parallel_tool_calls(self):
        page = self.session(self.page)
        other = self.session()
        self.submit(page, 'Alpha parallel')
        expect(page.locator('.tool.running')).to_have_count(2)
        snapshot = self.context.request.get(page.url.replace('/sessions/', '/api/sessions/')).json()
        call = next(block['background_id'] for block in snapshot['change']['snapshot']['blocks'] if block.get('background_id'))
        wrong = self.context.request.post(other.url.replace('/sessions/', '/api/sessions/') + '/background',
                                          data={'session_id': other.url.rsplit('/', 1)[1], 'call_id': call})
        self.assertEqual(wrong.status, 409)
        expect(page.locator('.tool.running')).to_have_count(2)
        page.click('#activity-toggle')
        page.locator('#activity-list .background-tool').first.click()
        expect(page.locator('.tool .background-tool')).to_have_count(1)
        expect(page.locator('.tool.running')).to_have_count(2)
        expect(page.locator('#activity-list .active-call')).to_have_count(2)
        page.click('#activity-close')
        self.assertEqual(len(self.requests), 1, 'The second foreground tool still blocks the batch')
        (self.home / 'Alpha-release-1').touch()
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.assistant .body').last).to_have_text('Alpha finished.')
        expect(page.locator('#connection')).to_have_text('Running')
        (self.home / 'Alpha-release-0').touch()
        expect(page.locator('#connection')).to_have_text('Ready')

    def test_interactive_shells_stay_active_and_clickable_between_turns(self):
        home, page = self.page, self.session()
        status = self.browser_status(home, page)
        self.pause_browser_polling(home)
        self.submit(page, 'Alpha shell')
        for indicator in [page.locator('#connection'), status]:
            expect(indicator).to_have_text('Running')
            expect(indicator).to_have_attribute('data-state', 'busy')
            expect(indicator).to_have_attribute('data-busy', 'false')
            self.assertEqual(indicator.evaluate("n => getComputedStyle(n, '::before').animationName"), 'activity-pulse')
        expect(page.locator('.tool.running')).to_have_count(1)
        self.assertEqual(page.locator('.tool.running').evaluate("n => getComputedStyle(n, '::after').animationName"), 'running-border')
        page.emulate_media(reduced_motion='reduce')
        self.assertEqual(page.locator('.tool.running').evaluate("n => getComputedStyle(n, '::after').animationName"), 'none')
        page.emulate_media(reduced_motion='no-preference')
        self.submit(page, 'Alpha read marker')
        expect(page.locator('#model')).to_be_enabled()
        self.assertEqual((self.home / 'Alpha-marker').read_text(), 'Alpha')
        expect(page.locator('.tool.running')).to_have_count(1)
        page.click('#compact')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.tool.running')).to_have_count(1)
        page.reload()
        page.click('#activity-toggle')
        expect(page.locator('#activity-list .active-call')).to_have_count(1)
        page.locator('#activity-list .active-call').click()
        expect(page.locator('.tool.running')).to_have_attribute('open', '')
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(2)
        for indicator in [page.locator('#connection'), status]:
            expect(indicator).to_have_text('Running')
            expect(indicator).to_have_attribute('data-busy', 'true')
        page.click('#cancel')
        for indicator in [page.locator('#connection'), status]:
            expect(indicator).to_have_text('Running')
            expect(indicator).to_have_attribute('data-busy', 'false')

    def test_disconnected_activity_is_unknown_and_recovers_without_rerunning(self):
        home, page = self.page, self.session()
        status = self.browser_status(home, page)
        self.pause_browser_polling(home)
        self.submit(page, 'Alpha generate')
        expect(status).to_have_attribute('data-busy', 'true')
        self.assertTrue(self.request_received.wait(10), 'Generation must reach the provider before disconnecting')
        self.stop(self.process)
        for indicator in [page.locator('#connection'), home.locator('#connection'), status]:
            expect(indicator).to_have_text('Reconnecting…')
            expect(indicator).to_have_attribute('data-busy', 'false')
        self.generation_releases['Alpha'].set()
        self.process, _launch = self.launch(port=urlsplit(self.origin).port)
        expect(home.locator('#connection')).to_have_text('Live', timeout=15000)
        expect(page.locator('#connection')).to_have_text('Ready', timeout=15000)
        expect(status).to_have_text('Ready')
        expect(status).to_have_attribute('data-busy', 'false')
        self.assertEqual(len(self.requests), 1)

    def test_failed_generation_stops_both_activity_indicators(self):
        home, page = self.page, self.session()
        status = self.browser_status(home, page)
        self.pause_browser_polling(home)
        self.submit(page, 'Alpha fail')
        for indicator in [page.locator('#connection'), status]:
            expect(indicator).to_have_text('Stopped')
            expect(indicator).to_have_attribute('data-busy', 'false')
            expect(indicator).to_have_attribute('data-state', 'attention')

    def test_completion_during_a_slow_browser_refresh_is_not_lost(self):
        home, page = self.page, self.session()
        status = self.browser_status(home, page)
        self.pause_browser_polling(home)
        self.submit(page, 'Alpha wait')
        expect(status).to_have_attribute('data-busy', 'true')
        held = []
        intercepted = False
        def hold_once(route):
            nonlocal intercepted
            if intercepted:
                route.continue_()
            else:
                intercepted = True
                held.append((route, route.fetch()))
        home.route('**/api/sessions?archived=false', hold_once)
        home.evaluate("document.dispatchEvent(new Event('visibilitychange'))")
        deadline = time.monotonic() + 5
        while not held and time.monotonic() < deadline:
            home.wait_for_timeout(10)
        self.assertEqual(len(held), 1)
        route, response = held.pop()
        self.assertTrue(response.json()[0]['busy'])
        page.click('#cancel')
        expect(page.locator('#connection')).to_have_attribute('data-busy', 'false')
        home.wait_for_timeout(100)
        route.fulfill(response=response)
        expect(status).to_have_attribute('data-busy', 'false', timeout=2000)

    def test_new_opens_an_independent_tab_and_preserves_the_running_session_and_draft(self):
        with self.context.expect_page() as opened:
            self.page.click('#new-session')
        page = opened.value
        page.wait_for_url(re.compile(r'.*/sessions/[a-f0-9]{32}$'))
        expect(page.locator('#model')).to_be_enabled()
        self.assertEqual(self.page.url, self.origin + '/profiles/default/')
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        page.fill('#prompt', 'Keep this draft')
        previous = page.url
        with self.context.expect_page() as opened:
            page.click('#new-session')
        fresh = opened.value
        fresh.wait_for_url(re.compile(r'.*/sessions/[a-f0-9]{32}$'))
        expect(fresh.locator('#model')).to_be_enabled()
        self.assertNotEqual(fresh.url, previous)
        self.assertEqual(page.url, previous)
        expect(page.locator('#prompt')).to_have_value('Keep this draft')
        expect(page.locator('.tool.running')).to_have_count(1)
        expect(fresh.locator('.user')).to_have_count(0)
        self.assertIsNone(fresh.evaluate('window.opener'))
        page.click('#cancel')
        expect(page.locator('#model')).to_be_enabled()
        with self.context.expect_page() as opened:
            page.fill('#prompt', '/new')
            page.press('#prompt', 'Enter')
        opened.value.wait_for_url(re.compile(r'.*/sessions/[a-f0-9]{32}$'))
        expect(page.locator('#prompt')).to_have_value('')
        self.assertEqual(page.url, previous)

    def test_new_tab_retries_a_lost_creation_response_without_duplicate_sessions(self):
        attempts = []
        def lose_once(route):
            if route.request.method != 'POST':
                route.continue_()
                return
            attempts.append(route.request.post_data_json['request_id'])
            response = route.fetch()
            if len(attempts) == 1:
                route.abort()
            else:
                route.fulfill(response=response)
        self.context.route('**/api/sessions', lose_once)
        with self.context.expect_page() as opened:
            self.page.click('#new-session')
        page = opened.value
        expect(page.locator('#retry')).to_be_visible()
        self.assertEqual(self.page.url, self.origin + '/profiles/default/')
        page.reload()
        page.wait_for_url(re.compile(r'.*/sessions/[a-f0-9]{32}$'))
        expect(page.locator('#model')).to_be_enabled()
        self.assertEqual(len(attempts), 2)
        self.assertEqual(attempts[0], attempts[1])
        sessions = self.context.request.get(self.origin + '/api/sessions').json()
        self.assertEqual(len(sessions), 1)

    def test_assistant_heading_precedes_tool_first_output_live_after_reload_and_restart(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        heading = page.locator('#transcript > .message-header')
        expect(heading.locator('.role')).to_have_text('ASSISTANT')
        self.assertTrue(page.locator('.tool').evaluate("tool => tool.previousElementSibling.querySelector('.role').textContent === 'ASSISTANT'"))
        expect(heading.locator('time')).to_have_text(page.locator('.user time').inner_text())
        page.reload()
        expect(heading.locator('.role')).to_have_text('ASSISTANT')
        page.click('#cancel')
        expect(page.locator('#model')).to_be_enabled()
        session_url = page.url
        port = urlsplit(self.origin).port
        self.stop(self.process)
        self.process, launch = self.launch(port=port)
        page.goto(session_url)
        expect(heading.locator('.role')).to_have_text('ASSISTANT')
        expect(page.locator('.tool')).to_have_count(1)

    def test_loopback_has_no_login_and_rejects_foreign_web_origins(self):
        self.assertEqual(self.context.cookies(), [])
        anonymous = self.playwright.request.new_context()
        try:
            for path in ['/', '/app.js', '/api/sessions', '/files/pixel.png', '/api/image?source=pixel.png']:
                response = anonymous.get(self.origin + path)
                self.assertEqual(response.status, 200, path)
                self.assertNotIn('set-cookie', response.headers)
                self.assertNotIn('www-authenticate', response.headers)
            self.assertEqual(anonymous.head(self.origin + '/files/pixel.png').status, 200)
            self.assertEqual(anonymous.get(self.origin + '/auth?token=old').status, 404)
            self.assertEqual(anonymous.post(self.origin + '/api/markdown', data={'text': '**local**'}).status, 200)
            for origin in ['https://other.example', 'null', 'http://localhost:1']:
                self.assertEqual(anonymous.post(self.origin + '/api/markdown',
                    headers={'Origin': origin}, data={'text': 'no'}).status, 403)
            for site in ['cross-site', 'same-site']:
                self.assertEqual(anonymous.get(self.origin + '/files/pixel.png',
                    headers={'Sec-Fetch-Site': site}).status, 403)
            for host in ['other.example:8765', '192.168.1.10:8765', 'localhost.evil:8765']:
                self.assertEqual(anonymous.get(self.origin + '/api/sessions', headers={'Host': host}).status, 403)
            self.assertEqual(anonymous.get(self.origin + '/api/sessions',
                headers={'Cookie': 'myco_8765=old', 'Authorization': 'Bearer old'}).status, 200)
        finally:
            anonymous.dispose()

    def test_forwarded_port_supports_actions_events_and_images_without_login(self):
        destination = ('127.0.0.1', urlsplit(self.origin).port)

        class Forward(socketserver.BaseRequestHandler):
            def handle(self):
                # Relay TCP bytes like ssh -L; preserve Host and Origin.
                with socket.create_connection(destination) as upstream:
                    with selectors.DefaultSelector() as selector:
                        selector.register(self.request, selectors.EVENT_READ, upstream)
                        selector.register(upstream, selectors.EVENT_READ, self.request)
                        while True:
                            for key, _ in selector.select():
                                data = key.fileobj.recv(65536)
                                if not data:
                                    return
                                key.data.sendall(data)

        forwarder = socketserver.ThreadingTCPServer(('127.0.0.1', 0), Forward)
        forwarder.daemon_threads = True
        self.addCleanup(forwarder.server_close)
        self.addCleanup(forwarder.shutdown)
        threading.Thread(target=forwarder.serve_forever, daemon=True).start()
        port = forwarder.server_address[1]
        origin = f'http://127.0.0.1:{port}'
        self.page.goto(origin)
        self.assertEqual(self.page.url, origin + '/profiles/default/')
        self.assertEqual(self.context.cookies(), [])
        with self.context.expect_page() as opened:
            self.page.click('#new-session')
        self.page = opened.value
        self.page.wait_for_url(re.compile(re.escape(origin) + r'/profiles/default/sessions/[a-f0-9]{32}$'))
        expect(self.page.locator('#model')).to_be_enabled()
        self.submit(self.page, 'Alpha markdown')
        expect(self.page.locator('.assistant .markdown')).to_contain_text('Paragraph 23')
        expect(self.page.locator('.assistant img')).to_have_js_property('naturalWidth', 1)
        self.page.reload()
        expect(self.page.locator('.assistant img')).to_have_js_property('naturalWidth', 1)

    def test_workspace_links_and_svg_images_are_mapped_without_login(self):
        path = self.workspace / 'plot & space.svg'
        path.write_text('<svg xmlns="http://www.w3.org/2000/svg" width="12" height="8"><rect width="12" height="8" fill="cyan"/></svg>')
        (self.workspace / 'notes.txt').write_text('workspace notes')
        source = f'![relative](plot%20%26%20space.svg) ![absolute](<{path}>) ![file](<{path.as_uri()}>)\n\n[Notes](notes.txt)'
        response = self.context.request.post(self.origin + '/api/markdown',
            headers={'Origin': self.origin}, data={'text': source})
        self.assertEqual(response.status, 200)
        self.assertIn('href="/profiles/default/files/notes.txt"', response.text())
        self.assertEqual(response.text().count('src="/profiles/default/files/'), 3)
        self.page.evaluate('html => { const box = document.createElement("div"); box.id="file-preview"; box.innerHTML=html; document.body.append(box); }', response.text())
        for image in self.page.locator('#file-preview img').all():
            expect(image).to_have_js_property('naturalWidth', 12)
        self.page.get_by_role('link', name='Notes', exact=True).click()
        expect(self.page.locator('body')).to_contain_text('workspace notes')

    def test_workspace_files_reject_escape_and_preserve_range_and_head_semantics(self):
        (self.workspace / 'bytes.bin').write_bytes(b'0123456789')
        with tempfile.TemporaryDirectory(prefix='myco-outside-') as outside:
            secret = Path(outside) / 'secret.txt'
            secret.write_text('outside workspace')
            (self.workspace / 'escape').symlink_to(outside, target_is_directory=True)
            paths = ['/files/escape/secret.txt', '/files/..%2F' + Path(outside).name + '/secret.txt', '/files/%2Fetc/passwd']
            for path in paths:
                self.assertIn(self.context.request.get(self.origin + path).status, [403, 404], path)
        url = self.origin + '/files/bytes.bin'
        response = self.context.request.get(url, headers={'Range': 'bytes=2-5'})
        self.assertEqual((response.status, response.body()), (206, b'2345'))
        self.assertEqual(response.headers['content-range'], 'bytes 2-5/10')
        self.assertEqual(self.context.request.get(url, headers={'Range': 'bytes=-3'}).body(), b'789')
        self.assertEqual(self.context.request.get(url, headers={'Range': 'bytes=20-'}).status, 416)
        response = self.context.request.head(url)
        self.assertEqual((response.status, response.body()), (200, b''))
        self.assertEqual(response.headers['content-length'], '10')
        for validator in ['"different-version"', 'Wed, 21 Oct 2015 07:28:00 GMT']:
            response = self.context.request.get(url, headers={'Range': 'bytes=2-5', 'If-Range': validator})
            self.assertEqual((response.status, response.body()), (200, b'0123456789'))
            self.assertNotIn('content-range', response.headers)
        for requested_range in ['bytes=2-5', 'bytes=20-', 'not a range']:
            response = self.context.request.head(url, headers={'Range': requested_range})
            self.assertEqual((response.status, response.body()), (200, b''))
            self.assertEqual(response.headers['content-length'], '10')
            self.assertNotIn('content-range', response.headers)
        self.assertEqual(self.context.request.post(url, headers={'Origin': self.origin}, data='replace').status, 405)
        self.assertEqual((self.workspace / 'bytes.bin').read_bytes(), b'0123456789')
        if hasattr(os, 'mkfifo'):
            os.mkfifo(self.workspace / 'pipe')
            self.assertEqual(self.context.request.get(self.origin + '/files/pipe', timeout=3000).status, 404)

    def test_workspace_html_displays_relative_assets_without_executing_scripts(self):
        folder = self.workspace / 'preview'
        folder.mkdir()
        (folder / 'style.css').write_text('h1 { color: rgb(12, 34, 56); }')
        (folder / 'index.html').write_text('<link rel="stylesheet" href="style.css"><h1>Preview</h1><img src="../pixel.png"><script>document.documentElement.dataset.executed="yes"; fetch("/api/sessions")</script>')
        page = self.context.new_page()
        response = page.goto(self.origin + '/files/preview')
        self.assertEqual(page.url, self.origin + '/profiles/default/files/preview/')
        self.assertEqual(response.status, 200)
        self.assertIn('sandbox allow-same-origin', response.headers['content-security-policy'])
        expect(page.locator('h1')).to_have_css('color', 'rgb(12, 34, 56)')
        expect(page.locator('img')).to_have_js_property('naturalWidth', 1)
        self.assertIsNone(page.locator('html').get_attribute('data-executed'))

    def test_settings_contains_sky_and_restores_focus_on_home_and_session(self):
        page = self.page
        for view in ["home", "session"]:
            if view == "session":
                self.session(page)
            with self.subTest(view=view):
                toggle = page.get_by_role("button", name="Settings", exact=True)
                dialog = page.get_by_role("dialog", name="Settings", exact=True)
                expect(toggle).to_be_visible()
                expect(page.locator(".toolbar").get_by_role("button", name="Sky", exact=True)).to_have_count(0)
                expect(page.locator("#sky-city")).to_be_hidden()
                for dismiss in ["close", "escape", "outside"]:
                    toggle.click()
                    expect(dialog).to_be_visible()
                    expect(toggle).to_have_attribute("aria-expanded", "true")
                    expect(dialog.get_by_role("heading", name="Sky", exact=True)).to_be_visible()
                    expect(page.get_by_role("button", name="Close settings")).to_be_focused()
                    page.keyboard.press("Tab")
                    expect(page.locator("#sky-city")).to_be_focused()
                    if dismiss == "close":
                        page.get_by_role("button", name="Close settings").click()
                    elif dismiss == "escape":
                        page.fill("#sky-city", "London")
                        page.keyboard.press("Escape")
                    else:
                        page.mouse.click(4, 4)
                    expect(dialog).to_be_hidden()
                    expect(toggle).to_have_attribute("aria-expanded", "false")
                    expect(toggle).to_be_focused()

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
        page.click("#settings-toggle")
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
        page.click("#settings-toggle")
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
        page.click("#settings-toggle")
        expect(page.locator("#sky-status")).to_contain_text("last available")
        expect(page.locator("#sky-error")).to_contain_text("Weather unavailable")
        page.clock.fast_forward(2 * 60 * 60 * 1000)
        expect(page.locator("#sky")).to_have_attribute("data-weather", "illustrated")
        expect(page.locator("#sky-coverage")).to_be_hidden()
        expect(page.locator("#sky-rain")).to_be_hidden()
        page.click("#settings-close")
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
        page.click("#settings-toggle")
        page.evaluate("() => { navigator.geolocation.getCurrentPosition = (_ok, fail) => fail({code: 1}); }")
        page.click("#sky-locate")
        expect(page.locator("#sky-error")).to_contain_text("Location unavailable")
        self.assertLessEqual(page.evaluate("document.documentElement.scrollWidth"), 390)
        rect = page.locator("#settings").bounding_box()
        self.assertGreaterEqual(rect["x"], 0)
        self.assertLessEqual(rect["x"] + rect["width"], 390)
        page.screenshot(path=str(self.artifacts / "sky-settings-mobile.png"))
        page.click("#settings-close")
        self.session(page)
        self.assertLessEqual(page.evaluate("document.documentElement.scrollWidth"), 390)

    def test_sky_assets_need_no_login_and_endpoints_validate_input(self):
        anonymous = self.playwright.request.new_context()
        try:
            for path in ["/clouds.js", "/cloud-renderer.js", "/aircraft.js", "/rain.js",
                         "/sky-weather.js", "/sky-noise.js", "/sky-light.js", "/sky-atmosphere.js", "/cloud-field.js", "/cloud-textures.js", "/settings.js"]:
                self.assertEqual(anonymous.get(self.origin + path).status, 200)
            for path in ["/api/sky/weather?latitude=91&longitude=0", "/api/sky/weather?latitude=nan&longitude=0", "/api/sky/locations?query=a"]:
                self.assertEqual(anonymous.get(self.origin + path).status, 400)
        finally:
            anonymous.dispose()

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
        page.click("#settings-toggle")
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
        page.click("#settings-toggle")
        expect(page.locator("#sky-conditions")).to_have_text("Heavy rain")
        page.click("#settings-close")
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

    def test_sky_lighting_follows_city_time_without_changing_cloud_shapes(self):
        page = self.session(self.page)
        page.clock.install()
        page.clock.set_fixed_time("2030-03-20T06:00:00Z")
        report = {"utc_offset_seconds": 10800, "current": {"time": int(time.time()), "interval": 900,
            "cloud_cover_low": 55, "cloud_cover_mid": 45, "cloud_cover_high": 60,
            "wind_speed_10m": 5, "wind_direction_10m": 260, "rain": 0, "showers": 0, "weather_code": 2}}
        self.context.route("**/api/sky/weather?*", lambda route: route.fulfill(json=report))
        page.evaluate("localStorage.setItem('myco.sky.location.v1', JSON.stringify({name: 'Test city', latitude: 50, longitude: 0}))")
        page.reload()
        sky, glow = page.locator("#sky"), page.locator(".sky-glow")
        expect(sky).to_have_attribute("data-weather", "live")
        expect(sky).to_have_attribute("data-clouds", "ready")
        glass = lambda: page.locator('.toolbar').evaluate("n => getComputedStyle(n).backgroundColor.match(/[\\d.]+/g).slice(0, 3).map(Number)")
        day_glass = glass()
        self.assertGreater(day_glass[2], day_glass[0], 'Daylight glass should carry the cool sky tint')
        emission = lambda: page.locator('#composer-frame').evaluate("n => getComputedStyle(n).getPropertyValue('--composer-emission').match(/[\\d.]+/g).map(Number)")
        day_light = emission()
        self.assertGreater(day_light[2], day_light[0])
        morning = float(glow.evaluate("n => n.style.getPropertyValue('--light-x').replace('%', '')"))
        canvas = page.locator(".cloud-low canvas").first
        # Compare rendered alpha and RGB independently: relighting must preserve
        # every edge and opening while visibly changing the direction of shading.
        fingerprint = """canvas => {
            const pixels = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data;
            let alpha = 0, color = 0;
            pixels.forEach((v, i) => { if (i % 4 === 3) alpha = (Math.imul(alpha, 31) + v) | 0;
                else color = (Math.imul(color, 31) + v) | 0; });
            return {alpha, color};
        }"""
        before = canvas.evaluate(fingerprint)
        page.clock.set_fixed_time("2030-03-20T12:00:00Z")
        page.clock.fast_forward(60000)
        expect(sky).to_have_attribute("data-clouds", "ready")
        afternoon = float(glow.evaluate("n => n.style.getPropertyValue('--light-x').replace('%', '')"))
        after = canvas.evaluate(fingerprint)
        self.assertLess(morning, 50)
        self.assertGreater(afternoon, 50)
        self.assertEqual(before["alpha"], after["alpha"])
        self.assertNotEqual(before["color"], after["color"])
        for hour in ['03', '15']:  # Dawn and dusk in the selected city's UTC+3 clock.
            page.clock.set_fixed_time(f"2030-03-20T{hour}:00:00Z")
            page.clock.fast_forward(60000)
            warm = glass()
            self.assertGreater(warm[0], warm[2], 'Twilight glass should follow the warm sky')
            self.assertGreater(emission()[0], emission()[2], 'Composer light follows dawn and dusk')
            surfaces = page.evaluate("""() => [getComputedStyle(document.body, '::before'),
                ...['#composer', '#settings', '#activity'].map(selector => getComputedStyle(document.querySelector(selector)))]
                .map(style => style.backgroundColor.match(/[\\d.]+/g).slice(0, 3).map(Number))""")
            self.assertTrue(all(color == warm for color in surfaces), 'Glass surfaces share the same tint')
        page.clock.set_fixed_time("2030-03-20T21:00:00Z")
        page.clock.fast_forward(60000)
        expect(sky).to_have_attribute("data-phase", "night")
        expect(sky).to_have_attribute("data-clouds", "ready")
        self.assertEqual(before["alpha"], canvas.evaluate(fingerprint)["alpha"])
        self.assertGreater(float(page.locator("#sky-stars").evaluate("n => n.style.opacity")), 0)
        night_glass = glass()
        self.assertGreater(night_glass[2], night_glass[0])
        self.assertLess(sum(night_glass), sum(day_glass), 'Night glass keeps its deeper blue tone')
        night_light = emission()
        self.assertGreater(night_light[2], night_light[0])
        self.assertLess(sum(night_light), sum(day_light))

    def test_sky_renderer_failure_keeps_the_fallback_and_conversation_usable(self):
        self.assert_sky_fallback_conversation("cloud-renderer.js")

    def test_sky_worker_import_failure_keeps_the_fallback_and_conversation_usable(self):
        self.assert_sky_fallback_conversation("cloud-field.js")

    def assert_sky_fallback_conversation(self, asset):
        self.context.route("**/" + asset, lambda route: route.fulfill(status=503, body="Unavailable"))
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
        expect(page.locator(".tool.running")).to_have_count(1)
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
        expect(page.locator("#queued-list .queued-content")).to_have_text(["Alpha markdown", "Alpha stream"])
        expect(page.locator(".user")).to_have_count(1)
        page.screenshot(path=str(self.artifacts / "queued-messages.png"))
        page.reload()
        expect(page.locator("#queued-list .queued-content")).to_have_text(["Alpha markdown", "Alpha stream"])
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

    def test_queue_editor_holds_position_preserves_drafts_and_sends_the_saved_text(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        for text in ['Alpha markdown', 'Alpha stream']:
            page.fill('#prompt', text)
            page.click('#send')
            expect(page.locator('#prompt')).to_have_value('')
        page.fill('#prompt', 'A separate draft')
        self.choose_image(page)
        page.locator('#queued-list li').first.get_by_role('button', name='Edit', exact=True).click()
        expect(page.locator('#prompt')).to_have_value('Alpha markdown')
        expect(page.locator('#attachments')).to_be_hidden()
        page.fill('#prompt', 'Beta markdown')
        (self.home / 'Alpha-release').touch()
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.user .body')).to_have_text(['Alpha wait'])
        expect(page.locator('#queued-list .queued-content')).to_have_text(['Alpha markdown', 'Alpha stream'])
        expect(page.locator('#queue-edit')).to_be_visible()
        page.screenshot(path=str(self.artifacts / 'editing-queue.png'))
        page.click('#send')
        expect(page.locator('#prompt')).to_have_value('A separate draft')
        expect(page.locator('#attachment-list img')).to_have_js_property('naturalWidth', 1)
        expect(page.locator('.user .body')).to_have_text(['Alpha wait', 'Beta markdown', 'Alpha stream'])
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#queued')).to_be_hidden()
        self.assertNotIn('Alpha markdown', self.turns)
        self.assertEqual(len(self.requests), 3)

    def test_editing_queued_images_and_unqueue_are_shared_across_tabs(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        page.fill('#prompt', 'Beta images')
        self.choose_image(page)
        page.click('#send')
        expect(page.locator('#queued-list img')).to_have_js_property('naturalWidth', 1)
        page.fill('#prompt', 'Do not send this')
        page.click('#send')
        expect(page.locator('#queued-list li')).to_have_count(2)
        other = self.context.new_page()
        other.goto(page.url)
        expect(other.locator('#queued-list li')).to_have_count(2)
        other.locator('#queued-list li').last.get_by_role('button', name='Unqueue').click()
        expect(page.locator('#queued-list li')).to_have_count(1)
        page.locator('#queued-list li').get_by_role('button', name='Edit', exact=True).click()
        expect(page.locator('#attachment-list img')).to_have_js_property('naturalWidth', 1)
        page.fill('#prompt', 'Alpha images')
        self.choose_image(page)
        expect(page.locator('#attachment-list img')).to_have_count(2)
        page.click('#send')
        expect(other.locator('#queued-list img')).to_have_count(2)
        page.locator('#queued-list li').get_by_role('button', name='Edit', exact=True).click()
        expect(page.locator('#attachment-list img')).to_have_count(2)
        page.locator('#attachment-list button').first.click()
        page.click('#send')
        expect(other.locator('#queued-list .queued-content')).to_have_text('Alpha images')
        expect(other.locator('#queued-list img')).to_have_count(1)
        (self.home / 'Alpha-release').touch()
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.user .body')).to_have_text(['Alpha wait', 'Alpha images'])
        self.assertEqual(len(self.image_urls(self.requests[-1])), 1)
        self.assertNotIn('Do not send this', json.dumps(self.requests))
        page.reload()
        expect(page.locator('.user img')).to_have_js_property('naturalWidth', 1)

    def test_held_edits_survive_reload_and_cancel_does_not_send_unfinished_edits(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        page.fill('#prompt', 'Alpha markdown')
        page.click('#send')
        page.locator('#queued-list li').get_by_role('button', name='Edit', exact=True).click()
        expect(page.locator('#queue-edit')).to_be_visible()
        expect(page.locator('#cancel')).to_have_text('Cancel run')
        page.click('#cancel')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.user .body')).to_have_text(['Alpha wait'])
        page.reload()
        expect(page.locator('#queued-list .queue-state')).to_have_text('Paused for editing')
        page.locator('#queued-list li').get_by_role('button', name='Resume', exact=True).click()
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.user .body')).to_have_text(['Alpha wait', 'Alpha markdown'])
        expect(page.locator('#queued')).to_be_hidden()
        self.assertEqual(len(self.requests), 2)

    def test_discarding_edits_restores_original_and_cross_tab_conflicts_keep_unsent_edits(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        page.fill('#prompt', 'Alpha markdown')
        page.click('#send')
        page.locator('#queued-list li').get_by_role('button', name='Edit', exact=True).click()
        expect(page.locator('#prompt')).to_have_value('Alpha markdown')
        page.fill('#prompt', 'Unfinished edit')
        page.click('#queue-edit-cancel')
        expect(page.locator('#queue-edit')).to_be_hidden()
        expect(page.locator('#prompt')).to_have_value('')
        expect(page.locator('#queued-list .queued-content')).to_have_text('Alpha markdown')
        page.locator('#queued-list li').get_by_role('button', name='Edit', exact=True).click()
        expect(page.locator('#queue-edit')).to_be_visible()
        page.fill('#prompt', 'My local changes')
        other = self.context.new_page()
        other.goto(page.url)
        other.locator('#queued-list li').get_by_role('button', name='Unqueue').click()
        expect(page.locator('#queued')).to_be_hidden()
        expect(page.locator('#prompt')).to_have_value('My local changes')
        expect(page.locator('#send')).to_have_text('Send as new')
        page.press('#prompt', 'Enter')
        expect(page.locator('#error')).to_contain_text('Click Send as new')
        self.assertEqual(len(self.requests), 1)
        page.click('#send')
        expect(page.locator('#queue-edit')).to_be_hidden()
        expect(page.locator('#queued-list .queued-content')).to_have_text('My local changes')
        page.click('#cancel')
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('.user .body')).to_have_text(['Alpha wait', 'My local changes'])

    def test_lost_queue_save_response_retries_the_same_edit_after_delivery(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        page.fill('#prompt', 'Alpha markdown')
        page.click('#send')
        page.locator('#queued-list li').get_by_role('button', name='Edit', exact=True).click()
        expect(page.locator('#prompt')).to_have_value('Alpha markdown')
        page.fill('#prompt', 'Beta markdown')
        (self.home / 'Alpha-release').touch()
        expect(page.locator('#model')).to_be_enabled()
        attempts = []
        def lose_once(route):
            attempts.append(route.request.post_data_json)
            response = route.fetch()
            if len(attempts) == 1:
                route.abort()
            else:
                route.fulfill(response=response)
        page.route('**/api/sessions/*/action', lose_once)
        page.click('#send')
        expect(page.locator('#error')).to_contain_text('Your draft is still here')
        expect(page.locator('.user .body')).to_have_text(['Alpha wait', 'Beta markdown'])
        expect(page.locator('#model')).to_be_enabled()
        expect(page.locator('#send')).to_have_text('Retry save ↵')
        page.press('#prompt', 'Enter')
        expect(page.locator('#prompt')).to_have_value('')
        expect(page.locator('#queue-edit')).to_be_hidden()
        self.assertEqual(attempts[0], attempts[1])
        self.assertEqual(len(self.requests), 3)
        expect(page.locator('.user .body')).to_have_text(['Alpha wait', 'Beta markdown'])

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
        expect(page.locator("#queued-list .queued-content")).to_have_text(["Alpha markdown"])
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
        expect(page.locator("#queued-list .queued-content")).to_have_text(["Alpha markdown"])
        (self.home / "Alpha-release-0").touch()
        expect(page.locator(".tool.done")).to_have_count(1)
        page.wait_for_timeout(150)
        expect(page.locator("#queued-list .queued-content")).to_have_text(["Alpha markdown"])
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
        expect(page).to_have_title("Alpha wait · default · myco")
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
        expect(page).to_have_title("Renamed Alpha · default · myco")
        home = self.context.new_page()
        home.goto(self.origin)
        expect(home.locator(".session-name")).to_have_text("Renamed Alpha")
        page.reload()
        expect(page).to_have_title("Renamed Alpha · default · myco")
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

    def test_markdown_stays_whole_across_provider_text_parts_and_reload(self):
        self.turns['Alpha links'] = 1
        self.link_reply = ['# Re', 'view\n\n**Bo', 'ld** and _emphasis_.\n\n```rust\nlet ',
                           'value = 1;\n```\n\n- First\n  - Nested\n\n| Name | Value |\n| --- | --- |\n',
                           '| a\\|b | `literal*` |\n\n[Docs][source]\n\n[source]: https://example.com/docs']
        page = self.session(self.page)
        self.submit(page, 'Alpha links')
        expect(page.locator('#model')).to_be_enabled()
        for reload in [False, True]:
            if reload:
                page.reload()
            body = page.locator('.assistant .body')
            expect(body).to_have_count(1)
            expect(body.locator('h1')).to_have_text('Review')
            expect(body.locator('strong')).to_have_text('Bold')
            expect(body.locator('em')).to_have_text('emphasis')
            expect(body.locator('pre code')).to_have_text('let value = 1;\n')
            expect(body.locator('ul ul li')).to_have_text('Nested')
            expect(body.locator('td').first).to_have_text('a|b')
            expect(body.locator('td code')).to_have_text('literal*')
            expect(body.get_by_role('link', name='Docs')).to_have_attribute('href', 'https://example.com/docs')

    def test_table_split_across_text_parts_survives_streaming_completion_and_reload(self):
        self.turns['Alpha links'] = 1
        self.link_reply = ['| Component | Status | Count |\n| :--- | :',
                           '---: | ---: |\n| **Browser** | Rea',
                           'dy | 128 |\n| `a\\|b` | Pass | 7 |\n']
        self.event_gates = {index: threading.Event() for index in [1, 2, 3]}
        self.addCleanup(lambda: [gate.set() for gate in self.event_gates.values()])
        page = self.session(self.page)
        self.submit(page, 'Alpha links')
        body = page.locator('.assistant .body')
        expect(body).to_contain_text('| Component | Status | Count |')
        self.event_gates[1].set()
        table = body.locator('table')
        expect(table.locator('th')).to_have_text(['Component', 'Status', 'Count'])
        expect(table.locator('td').nth(1)).to_have_text('Rea')
        self.event_gates[2].set()
        expect(table.locator('td')).to_have_text(['Browser', 'Ready', '128', 'a|b', 'Pass', '7'])
        expect(page.locator('#model')).to_be_disabled()
        live = table.inner_html()
        self.event_gates[3].set()
        expect(page.locator('#model')).to_be_enabled()
        for reload in [False, True]:
            if reload:
                page.reload()
            expect(body).to_have_count(1)
            expect(table.locator('td')).to_have_text(['Browser', 'Ready', '128', 'a|b', 'Pass', '7'])
            self.assertEqual(table.inner_html(), live)
            expect(table.locator('strong')).to_have_text('Browser')
            expect(table.locator('code')).to_have_text('a|b')
            self.assertEqual(table.locator('th').evaluate_all('nodes => nodes.map(n => getComputedStyle(n).textAlign)'),
                             ['left', 'center', 'right'])
        page.locator('.assistant').screenshot(path=str(self.artifacts / 'table-desktop.png'))
        page.set_viewport_size({'width': 390, 'height': 844})
        page.locator('.assistant').screenshot(path=str(self.artifacts / 'table-mobile.png'))

    def test_table_line_breaks_survive_streaming_and_reload_without_rendering_other_html(self):
        self.turns['Alpha links'] = 1
        self.link_reply = ['| Case | Value |\n| --- | --- |\n| Break | First<br',
                           '/>Second |\n| Escaped pipe | a\\|b |\n| Path | src/browser/markdown.rs |\n',
                           '| Code | `<br/>` |\n| HTML | <br onclick="alert(1)"> |\n']
        self.event_gates = {index: threading.Event() for index in [1, 2, 3]}
        self.addCleanup(lambda: [gate.set() for gate in self.event_gates.values()])
        page = self.session(self.page)
        self.submit(page, 'Alpha links')
        table = page.locator('.assistant table')
        expect(table.locator('td').last).to_have_text('First<br')
        self.event_gates[1].set()
        expect(table.locator('br')).to_have_count(1)
        self.event_gates[2].set()
        expect(table.locator('tr')).to_have_count(6)
        self.event_gates[3].set()
        expect(page.locator('#model')).to_be_enabled()
        for reload in [False, True]:
            if reload:
                page.reload()
            expect(table.locator('br')).to_have_count(1)
            self.assertEqual(table.locator('td').nth(1).inner_text(), 'First\nSecond')
            expect(table.locator('td').nth(3)).to_have_text('a|b')
            expect(table.locator('td').nth(5)).to_have_text('src/browser/markdown.rs')
            expect(table.locator('code')).to_have_text('<br/>')
            expect(table.locator('td').last).to_have_text('<br onclick="alert(1)">')
            expect(table.locator('[onclick]')).to_have_count(0)
        page.locator('.assistant').screenshot(path=str(self.artifacts / 'table-desktop.png'))
        page.set_viewport_size({'width': 390, 'height': 844})
        page.locator('.assistant').screenshot(path=str(self.artifacts / 'table-mobile.png'))

    def test_footnotes_stay_with_their_message_and_cannot_replace_composer_ids(self):
        page = self.session(self.page)
        for name in ['Alpha', 'Beta']:
            self.turns[name + ' links'] = 1
            self.link_reply = f'{name} note[^prompt].\n\n[^prompt]: {name} footnote.\n\n[External](https://example.com/#intro)'
            self.submit(page, name + ' links')
            expect(page.locator('#model')).to_be_enabled()
            expect(page.locator('.footnote-definition')).to_have_count(1 if name == 'Alpha' else 2)
            expect(page.locator('#prompt')).to_have_count(1)
            expect(page.locator('#prompt')).to_be_editable()
        for reload in [False, True]:
            if reload:
                page.reload()
            notes = page.locator('.footnote-definition')
            expect(notes).to_have_count(2)
            ids = notes.evaluate_all('nodes => nodes.map(n => n.id)')
            self.assertEqual(len(set(ids)), 2)
            links = page.locator('.footnote-reference a')
            for link in links.all():
                expect(link).to_have_js_property('target', '')
                self.assertTrue(link.evaluate('n => n.closest(".body").contains(document.getElementById(n.hash.slice(1)))'))
            links.last.click()
            self.assertEqual(page.url.split('#')[-1], ids[-1])
            self.assertEqual(len(self.context.pages), 1, 'footnotes must stay in the current tab')
            for link in page.get_by_role('link', name='External').all():
                expect(link).to_have_attribute('target', '_blank')

    def test_failed_markdown_render_retries_on_the_next_snapshot(self):
        self.event_gates = {1: threading.Event()}
        self.addCleanup(self.event_gates[1].set)
        page = self.session(self.page)
        page.route("**/api/markdown", lambda route: route.abort(), times=1)
        with page.expect_event("requestfailed", predicate=lambda request: request.url.endswith("/api/markdown")):
            self.submit(page, "Alpha markdown")
        expect(page.locator('.markdown')).to_have_text('Formatting unavailable. Reload to retry.')
        body = page.locator(".markdown").element_handle()
        self.event_gates[1].set()
        expect(page.locator(".markdown table")).to_have_count(1)
        expect(page.locator("#model")).to_be_enabled()
        self.assertTrue(body.evaluate("node => node.isConnected"))

    def test_reloaded_markdown_arrives_formatted_without_extra_requests(self):
        profile = self.add_profile()
        (profile / 'workspace/pixel.png').write_bytes((self.home / 'pixel.png').read_bytes())
        self.turns['Alpha links'] = 1
        self.link_reply = '# Review\n\n**Ready**\n\n| File | State |\n| --- | --- |\n| src/main.rs | Checked |\n\n![Pixel](pixel.png)\n\n[Notes](notes.txt)\n\n<script>window.injected = true</script>'
        page = self.session(self.page, profile='research')
        self.submit(page, 'Alpha links')
        expect(page.locator('.assistant table')).to_be_visible()
        expect(page.locator('#model')).to_be_enabled()
        requests = []
        page.route('**/api/markdown', lambda route: (requests.append(route.request.url), route.abort()))
        page.add_init_script("""window.rawPaints = [];
            new MutationObserver(() => {
                for (const node of document.querySelectorAll('.assistant .body')) {
                    if (node.textContent.startsWith('# Review')) window.rawPaints.push(node.textContent);
                }
            }).observe(document, {subtree: true, childList: true, characterData: true});""")
        for restart in [False, True]:
            if restart:
                self.stop(self.process)
                self.process, _ = self.launch(port=urlsplit(self.origin).port)
            page.reload()
            body = page.locator('.assistant .body')
            expect(body.locator('h1')).to_have_text('Review')
            expect(body.locator('strong')).to_have_text('Ready')
            expect(body.locator('td')).to_have_text(['src/main.rs', 'Checked'])
            expect(body.locator('img')).to_have_attribute('src', '/profiles/research/files/pixel.png')
            expect(body.locator('img')).to_have_js_property('naturalWidth', 1)
            expect(body.get_by_role('link', name='Notes')).to_have_attribute('href', '/profiles/research/files/notes.txt')
            expect(body).to_contain_text('<script>window.injected = true</script>')
            self.assertFalse(page.evaluate('Boolean(window.injected)'))
            self.assertEqual(page.evaluate('window.rawPaints'), [])
            self.assertEqual(requests, [], 'Saved Markdown must not need per-message formatting requests')
            snapshot = self.context.request.get(page.url.replace('/sessions/', '/api/sessions/')).json()['change']['snapshot']
            assistant = next(block for block in snapshot['blocks'] if block.get('role') == 'assistant')
            user = next(block for block in snapshot['blocks'] if block.get('role') == 'user')
            self.assertEqual(assistant['text'], self.link_reply)
            self.assertIn('<h1>Review</h1>', assistant['html'])
            self.assertNotIn('html', user)

    def test_streaming_coalesces_renders_and_late_html_cannot_overwrite_a_snapshot(self):
        page = self.session(self.page)
        expect(page.locator(".notice")).to_contain_text("prelude directory unreadable")
        held = []
        self.addCleanup(lambda: [route.abort() for route in held])
        page.route("**/api/markdown", lambda route: held.append(route))
        self.submit(page, "Alpha stream")
        expect(page.locator("#model")).to_be_enabled()
        page.wait_for_timeout(200)
        self.assertEqual(len(held), 1, "Only one render of the streaming block may be in flight")
        expect(page.locator(".markdown p").last).to_have_text("Chunk 29.")
        first = held.pop()
        first.fulfill(response=first.fetch())
        page.wait_for_timeout(150)
        expect(page.locator(".markdown p").last).to_have_text("Chunk 29.")
        self.assertEqual(held, [], 'The snapshot already supplied the latest rendered output')

    def test_26_concurrent_sessions_do_not_replay_unobserved_histories(self):
        home = self.page
        expect(home.locator('#connection')).to_have_text('Live')
        home.evaluate("""Object.defineProperty(document, 'hidden', {configurable: true, value: true});
            document.dispatchEvent(new Event('visibilitychange'));""")
        self.stream_chunks = 100
        release = threading.Event()
        self.event_gates = {0: release}
        self.addCleanup(release.set)
        self.context.add_init_script("""window.liveMessages = [];
            const Shared = window.SharedWorker;
            window.SharedWorker = function(...args) {
                const worker = new Shared(...args);
                worker.port.addEventListener('message', ({data}) => {
                    if (data.update) window.liveMessages.push({kind: data.kind,
                        change: data.update.change.kind, session: data.update.session_id});
                });
                return worker;
            };""")
        page = self.session()
        ids = [page.url.rsplit('/', 1)[1]]
        for _ in range(25):
            response = self.context.request.post(self.origin + '/api/sessions', data={'request_id': str(uuid.uuid4())})
            self.assertEqual(response.status, 200)
            ids.append(response.json()['id'])
        for session_id in ids:
            response = self.context.request.post(self.origin + '/api/sessions/' + session_id + '/action',
                data={'request_id': str(uuid.uuid4()), 'session_id': session_id,
                      'action': {'kind': 'submit', 'text': 'Alpha stream\n' + 'History. ' * 110_000}})
            self.assertEqual(response.status, 202)
        expect(home.locator('.session-status[data-busy="true"]')).to_have_count(26, timeout=30000)
        deadline = time.monotonic() + 15
        while len(self.requests) < 26 and time.monotonic() < deadline:
            page.wait_for_timeout(20)
        self.assertEqual(len(self.requests), 26)
        listings = []
        home.on('request', lambda request: listings.append(request.url) if '/api/sessions?' in request.url else None)
        release.set()
        expect(page.locator('.assistant .body p').last).to_have_text('Chunk 99.', timeout=30000)
        expect(page.locator('#model')).to_be_enabled()
        expect(home.locator('.session-status[data-busy="false"]')).to_have_count(26, timeout=30000)
        messages = page.evaluate('window.liveMessages')
        self.assertEqual({message['session'] for message in messages}, {ids[0]})
        self.assertTrue(any(message['change'] == 'append' for message in messages))
        self.assertFalse(any(message['kind'] == 'update' and message['change'] == 'snapshot' for message in messages))
        self.assertLessEqual(len(listings), 2 * len(ids), 'list refreshes must stay bounded as sessions finish')

    def test_sessions_isolate_models_tools_and_cancellation(self):
        alpha = self.session(self.page)
        beta = self.session()
        beta.select_option("#model", "second")
        expect(beta.locator("#model")).to_be_enabled()
        for page, name in [(alpha, "Alpha"), (beta, "Beta")]:
            self.submit(page, f"{name} shell")
            expect(page.locator("#activity-list .active-call")).to_have_count(1)
            expect(page.locator("#model")).to_be_enabled()
            self.submit(page, f"{name} wait")
            expect(page.locator(".tool.running")).to_have_count(2)
            expect(page.locator("#model")).to_be_disabled()
        duplicate = self.context.new_page()
        duplicate.goto(alpha.url)
        expect(duplicate.locator(".tool.running")).to_have_count(2)
        expect(duplicate.locator("#model")).to_have_value("first")
        beta.click("#cancel")
        expect(beta.locator(".tool.failed")).to_have_count(1)
        expect(beta.locator("#model")).to_be_enabled()
        expect(alpha.locator(".tool.running")).to_have_count(2)
        (self.home / "Alpha-release").touch()
        for page in [alpha, duplicate]:
            expect(page.locator("#model")).to_be_enabled()
            expect(page.locator(".tool.done")).to_have_count(1)
            expect(page.locator(".tool.running")).to_have_count(1)
        self.assertTrue((self.home / "Alpha-done").exists())
        self.assertFalse((self.home / "Beta-done").exists())
        for page, name in [(alpha, "Alpha"), (beta, "Beta")]:
            self.submit(page, f"{name} read marker")
            expect(page.locator(".tool")).to_have_count(3)
            expect(page.locator("#model")).to_be_enabled()
            self.assertEqual((self.home / f"{name}-marker").read_text(), name)
        self.assertEqual({request["model"] for request in self.requests}, {"first", "second"})

    def test_cancelled_snapshot_and_lost_recovery_event_restore_sending_without_reload(self):
        page = self.session()
        cdp = self.browser.new_browser_cdp_session()
        self.addCleanup(cdp.detach)
        targets = cdp.send('Target.getTargets')['targetInfos']
        worker = next(target for target in targets
                      if target['type'] == 'shared_worker' and target['url'].startswith(self.origin))
        worker_session = cdp.send('Target.attachToTarget', {'targetId': worker['targetId']})['sessionId']
        # Exercise the shipped worker: a cancelled snapshot after a disconnection,
        # with the reconnect notification lost to overflow before the resync.
        expression = """(() => {
            const originalFetch = fetch;
            fetch = () => { fetch = originalFetch; return Promise.reject(new Error('cancelled')); };
            stream.onmessage({data: JSON.stringify({kind:'connection', profile:'default', connected:false})});
            stream.onmessage({data: JSON.stringify({kind:'resync'})});
        })()"""
        cdp.send('Target.sendMessageToTarget', {'sessionId': worker_session, 'message': json.dumps({
            'id': 1, 'method': 'Runtime.evaluate', 'params': {'expression': expression}})})
        expect(page.locator('#error')).to_have_text('cancelled')
        expect(page.locator('#send')).to_be_enabled(timeout=8000)
        expect(page.locator('#error')).to_be_hidden()
        self.generation_releases['Alpha'].set()
        self.submit(page, 'Alpha generate')
        expect(page.locator('.assistant .body').last).to_have_text('Alpha finished.')
        expect(page.locator('#model')).to_be_enabled()
        self.assertEqual(len(self.requests), 1)

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

    def test_markdown_tables_keep_alignment_and_allow_keyboard_scrolling_without_widening_the_page(self):
        self.link_reply = (
            '| Check | State | Count |\n| :--- | :---: | ---: |\n'
            '| Browser | **Ready** | 128 |\n| Runtime | Ready | 7 |\n\n'
            '| Component | Revision | Artifact |\n| --- | --- | --- |\n'
            '| Browser | `0123456789abcdef0123456789abcdef01234567` | browser-desktop-preview.png |\n')
        page = self.session(self.page)
        violations = []
        page.on('console', lambda message: violations.append(message.text)
                if message.type == 'error' and 'Content Security Policy' in message.text else None)
        self.submit(page, 'Alpha links')
        tables = page.locator('.markdown table')
        expect(tables).to_have_count(2)
        self.assertEqual(tables.first.locator('th').evaluate_all('nodes => nodes.map(n => getComputedStyle(n).textAlign)'),
                         ['left', 'center', 'right'])
        expect(tables.first.locator('tbody tr').first.locator('td').nth(2)).to_have_css('text-align', 'right')
        expect(tables.first.locator('strong')).to_have_text('Ready')
        expect(tables.first.locator('th[scope="col"]')).to_have_count(3)
        self.assertLess(tables.first.bounding_box()['width'], page.locator('.assistant .body').last.bounding_box()['width'] / 2)
        page.set_viewport_size({'width': 390, 'height': 844})
        wide = page.get_by_role('region', name='Markdown table').last
        self.assertGreater(wide.evaluate('n => n.scrollWidth'), wide.evaluate('n => n.clientWidth'))
        self.assertLessEqual(page.evaluate('document.documentElement.scrollWidth'), 390)
        wide.focus()
        page.keyboard.press('ArrowRight')
        page.wait_for_function("() => document.querySelectorAll('.table-scroll')[1].scrollLeft > 0")
        expect(tables.last.locator('code')).to_have_text('0123456789abcdef0123456789abcdef01234567')
        self.assertTrue(tables.locator('.table-cell').evaluate_all('nodes => nodes.every(n => n.scrollWidth <= n.clientWidth + 1)'))
        page.reload()
        expect(page.get_by_role('region', name='Markdown table')).to_have_count(2)
        expect(tables.first.locator('th').nth(1)).to_have_css('text-align', 'center')
        self.assertEqual(violations, [])

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
    def test_session_archive_redirects_and_undo_survives_reload_within_its_profile(self):
        self.add_profile()
        default = self.session(self.page)
        session_id = default.url.rsplit('/', 1)[1]
        prefix = self.origin + '/profiles/research'
        response = self.context.request.post(prefix + '/api/sessions', data={'request_id': str(uuid.UUID(session_id))})
        self.assertEqual(response.status, 200)
        self.assertEqual(response.json()['id'], session_id)
        page = self.context.new_page()
        page.goto(prefix + '/sessions/' + session_id)
        expect(page.locator('#model')).to_be_enabled()
        self.turns['Beta images'] = 1
        self.submit(page, 'Beta images')
        expect(page.locator('#model')).to_be_enabled()
        for width in [320, 390, 1200]:
            page.set_viewport_size({'width': width, 'height': 850})
            expect(page.get_by_role('button', name='Archive', exact=True)).to_be_visible()
            self.assertLessEqual(page.evaluate('document.documentElement.scrollWidth'), width)
            page.screenshot(path=str(self.artifacts / f'session-{width}.png'))
        page.get_by_role('button', name='Archive', exact=True).click()
        expect(page).to_have_url(prefix + '/')
        notice = page.locator('#archive-notice')
        expect(notice).to_contain_text('Session Archived.')
        expect(page.locator('#session-list li')).to_have_count(0)
        self.assertEqual([s['id'] for s in self.context.request.get(self.origin + '/profiles/default/api/sessions').json()], [session_id])
        self.assertEqual([s['id'] for s in self.context.request.get(prefix + '/api/sessions?archived=true').json()], [session_id])
        page.reload()
        expect(notice).to_be_visible()
        page.set_viewport_size({'width': 390, 'height': 844})
        page.screenshot(path=str(self.artifacts / 'archived-mobile.png'))
        notice.get_by_role('button', name='Undo', exact=True).click()
        expect(notice).to_be_hidden()
        expect(page.locator('#session-list a')).to_have_attribute('href', f'/profiles/research/sessions/{session_id}')
        expect(page.locator('#session-list a')).to_be_focused()
        page.reload()
        expect(notice).to_be_hidden()
        page.locator('#session-list a').click()
        expect(page.locator('.assistant .body')).to_have_text('Beta finished.')

    def test_archive_and_undo_failures_keep_the_session_and_allow_retry(self):
        page = self.session(self.page)
        url = page.url
        page.fill('#prompt', 'Preserve this draft if archiving fails.')
        page.route('**/archive', lambda route: route.fulfill(status=500, body='Archive unavailable'), times=1)
        page.get_by_role('button', name='Archive', exact=True).click()
        expect(page.locator('#error')).to_have_text('Archive unavailable')
        expect(page).to_have_url(url)
        expect(page.locator('#prompt')).to_have_value('Preserve this draft if archiving fails.')
        expect(page.locator('#archive')).to_be_enabled()
        page.click('#archive')
        expect(page.locator('#archive-notice')).to_be_visible()
        page.route('**/archive', lambda route: route.fulfill(status=500, body='Restore unavailable'), times=1)
        page.click('#undo-archive')
        expect(page.locator('#error')).to_have_text('Restore unavailable')
        with page.expect_response(lambda response: '/api/sessions?' in response.url):
            page.evaluate("document.dispatchEvent(new Event('visibilitychange'))")
        expect(page.locator('#error')).to_have_text('Restore unavailable')
        expect(page.locator('#archive-notice')).to_be_visible()
        expect(page.locator('#undo-archive')).to_be_enabled()
        expect(page.locator('#session-list li')).to_have_count(0)
        page.click('#undo-archive')
        expect(page.locator('#archive-notice')).to_be_hidden()
        expect(page.locator('#session-list a')).to_have_attribute('href', urlsplit(url).path)

    def test_archiving_from_the_session_keeps_tools_and_queued_messages_running(self):
        page = self.session(self.page)
        self.submit(page, 'Alpha wait')
        expect(page.locator('.tool.running')).to_have_count(1)
        self.turns['Beta images'] = 1
        page.fill('#prompt', 'Beta images')
        page.press('#prompt', 'Enter')
        expect(page.locator('#queued-list .queued-content')).to_have_text('Beta images')
        page.click('#archive')
        expect(page.locator('#archive-notice')).to_be_visible()
        page.click('#undo-archive')
        expect(page.locator('#session-list .session-status')).to_have_text('Running')
        page.locator('#session-list a').click()
        expect(page.locator('.tool.running')).to_have_count(1)
        expect(page.locator('#queued-list .queued-content')).to_have_text('Beta images')
        (self.home / 'Alpha-release').touch()
        expect(page.locator('.assistant .body').last).to_have_text('Beta finished.')
        expect(page.locator('#model')).to_be_enabled()

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
