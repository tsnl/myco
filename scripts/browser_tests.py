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


def tool(arguments, name="bash"):
    return [
        {"type": "response.output_item.added", "output_index": 0,
         "item": {"type": "function_call", "name": name,
                  "call_id": str(uuid.uuid4()), "arguments": ""}},
        {"type": "response.function_call_arguments.done", "output_index": 0,
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
            match = re.search(r"(Alpha|Beta) (shell|read marker|wait|stream|markdown|rename)\b", content)
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

    def test_queued_messages_survive_refresh_and_run_in_submission_order(self):
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

    def test_cancel_clears_queued_messages_without_submitting_them(self):
        page = self.session(self.page)
        self.submit(page, "Alpha wait")
        expect(page.locator(".tool.running")).to_have_count(1)
        page.fill("#prompt", "Alpha markdown")
        page.press("#prompt", "Enter")
        expect(page.locator("#queued-list li")).to_have_text(["Alpha markdown"])
        expect(page.locator("#cancel")).to_have_text("Cancel run & queue")
        page.click("#cancel")
        expect(page.locator("#model")).to_be_enabled()
        expect(page.locator("#queued")).to_be_hidden()
        expect(page.locator(".user")).to_have_count(1)
        expect(page.locator(".tool.failed")).to_have_count(1)
        self.assertNotIn("Alpha markdown", self.turns)

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
        page.wait_for_function("document.querySelector('.markdown img')?.naturalWidth === 1")
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
