#!/usr/bin/env python3
"""Check Android navigation against an isolated real Codex app-server, without model calls."""
import argparse
import http.server
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--serial", required=True)
parser.add_argument("--codex", default=shutil.which("codex"), help="local Codex binary with Unix app-server support")
parser.add_argument("--screenshots", type=pathlib.Path)
parser.add_argument("--test-class", default="wtf.fob.cs.navigation.NavigationTest", help="instrumentation test to run against the isolated app-server")
parser.add_argument("--queue-fixture", action="store_true", help="use a loopback-only fake model for queue and interrupt tests")
parser.add_argument("--container-fixture", action="store_true", help="run terminal tests in an isolated offline Docker container")
args = parser.parse_args()
if not args.codex:
    parser.error("provide --codex")
sdk = pathlib.Path(os.environ.get("ANDROID_SDK_ROOT", pathlib.Path.home() / "Library/Android/sdk"))
adb = [str(sdk / "platform-tools/adb"), "-s", args.serial]
package = "wtf.fob.cs"
stages = ("projects", "settings", "settings-dark", "drawer", "configure", "chats", "new-chat", "read-only", "launcher-settings", "queue", "activity", "selection", "terminal-vim", "terminal-tabs", "terminal-keyboard", "failure")


class QueueModel(http.server.BaseHTTPRequestHandler):
    """Keep turns active until fixture cleanup, without any external request."""

    def log_message(self, *_):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        with self.server.request_lock:
            self.server.request_count += 1
            number = self.server.request_count
        response = {"id": f"fixture-{number}", "object": "response", "status": "in_progress", "output": []}
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        try:
            self.wfile.write(("data: " + json.dumps({"type": "response.created", "response": response}) + "\n\n").encode())
            self.wfile.flush()
            self.server.stop_event.wait(180)
            response.update(status="completed", usage={"input_tokens": 1, "output_tokens": 0, "total_tokens": 1})
            self.wfile.write(("data: " + json.dumps({"type": "response.completed", "response": response}) + "\n\n").encode())
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


def device(*command, **kwargs):
    return subprocess.run(adb + list(command), capture_output=True, check=True, timeout=30, **kwargs)


with tempfile.TemporaryDirectory(prefix="cs-navigation-", dir="/tmp") as temporary:
    directory = pathlib.Path(temporary)
    log = (directory / "gateway.log").open("w+")
    env = dict(os.environ, CODEX_START_ANDROID_FIXTURE=temporary, CODEX_START_TEST_CODEX=args.codex)
    model = None
    if args.queue_fixture:
        model = http.server.ThreadingHTTPServer(("127.0.0.1", 0), QueueModel)
        model.request_lock = threading.Lock()
        model.request_count = 0
        model.stop_event = threading.Event()
        threading.Thread(target=model.serve_forever, daemon=True).start()
        env["CODEX_START_TEST_MODEL_URL"] = f"http://127.0.0.1:{model.server_port}/v1"
    fixture = "android_terminal_container_fixture" if args.container_fixture else "android_navigation_fixture"
    gateway = subprocess.Popen(["cargo", "+1.88.0", "test", "--locked", "-p", "codex-start", "--bin", "codex-start", f"remote::server::tests::{fixture}", "--", "--ignored", "--exact"], cwd=ROOT, env=env, stdout=log, stderr=log)
    port = None
    try:
        deadline = time.monotonic() + 240
        while not (directory / "connection.json").exists():
            if gateway.poll() is not None or time.monotonic() > deadline:
                log.seek(0)
                print(log.read())
                raise RuntimeError("Navigation gateway did not become ready")
            time.sleep(0.5)
        data = json.loads((directory / "connection.json").read_text())
        data["queueFixture"] = args.queue_fixture
        data["containerFixture"] = args.container_fixture
        port = data["port"]
        device("reverse", f"tcp:{port}", f"tcp:{port}")
        device("shell", "run-as", package, "mkdir", "-p", "files")
        device("shell", "run-as", package, "rm", "-f", "files/navigation-failure.txt", *(f"files/navigation-{stage}.png" for stage in stages))
        device("shell", f"run-as {package} sh -c 'cat > files/remote-navigation.json'", input=json.dumps(data).encode())
        device("shell", "input", "keyevent", "KEYCODE_WAKEUP")
        device("shell", "wm", "dismiss-keyguard")
        result = subprocess.run(adb + ["shell", "am", "instrument", "-w", "-r", "-e", "class", args.test_class, f"{package}.test/androidx.test.runner.AndroidJUnitRunner"], capture_output=True, text=True, timeout=300)
        print(result.stdout)
        if args.screenshots:
            args.screenshots.mkdir(parents=True, exist_ok=True)
            for stage in stages:
                screenshot = subprocess.run(adb + ["exec-out", "run-as", package, "cat", f"files/navigation-{stage}.png"], capture_output=True, timeout=30)
                if screenshot.returncode == 0 and screenshot.stdout.startswith(b"\x89PNG\r\n\x1a\n"):
                    (args.screenshots / f"{stage}.png").write_bytes(screenshot.stdout)
            failure = subprocess.run(adb + ["exec-out", "run-as", package, "cat", "files/navigation-failure.txt"], capture_output=True, timeout=30)
            if failure.returncode == 0:
                (args.screenshots / "failure.txt").write_bytes(failure.stdout)
        if result.returncode or "OK (1 test)" not in result.stdout:
            raise RuntimeError("Navigation acceptance failed")
    finally:
        if model:
            model.stop_event.set()
            model.shutdown()
            model.server_close()
        (directory / "stop").touch()
        try:
            gateway.wait(timeout=10)
        except subprocess.TimeoutExpired:
            gateway.terminate()
            gateway.wait(timeout=10)
        if port:
            subprocess.run(adb + ["reverse", "--remove", f"tcp:{port}"], capture_output=True, timeout=30)
        subprocess.run(adb + ["shell", "run-as", package, "rm", "-f", "files/remote-navigation.json"], capture_output=True, timeout=30)
