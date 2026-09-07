#!/usr/bin/env python3
"""Check direct notifications with a test gateway and a real Android process.

Uses synthetic completion events. It does not prove Codex task execution.
The temporary server uses the production TLS, registration, and event handlers.
"""
import argparse
import json
import os
import pathlib
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--serial", required=True)
parser.add_argument("--doze", action="store_true", help="force device idle temporarily; use an emulator or USB device")
parser.add_argument("--doze-only", action="store_true", help="test only Doze recovery without another process-death cycle")
args = parser.parse_args()
sdk = pathlib.Path(os.environ.get("ANDROID_SDK_ROOT", pathlib.Path.home() / "Library/Android/sdk"))
adb = [str(sdk / "platform-tools/adb"), "-s", args.serial]
package = "wtf.fob.cs"
fixture_path = "files/remote-background.json"


def device(*command, **kwargs):
    return subprocess.run(adb + list(command), capture_output=True, check=True, timeout=30, **kwargs)


def wait_for(predicate, timeout=60):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(0.5)
    raise RuntimeError("Android test stage did not become ready")


def instrument(method):
    result = subprocess.run(adb + ["shell", "am", "instrument", "-w", "-r", "-e", "class", f"wtf.fob.cs.app.BackgroundMonitoringTest#{method}", f"{package}.test/androidx.test.runner.AndroidJUnitRunner"], capture_output=True, text=True, timeout=120)
    if result.returncode or "OK (1 test)" not in result.stdout:
        print(result.stdout)
        raise RuntimeError(f"Background test failed: {method}")


def write_fixture(data):
    device("shell", f"run-as {package} sh -c 'cat > {fixture_path}'", input=json.dumps(data).encode())


def foreground_then_home():
    device("shell", "am", "start", "-W", "-n", f"{package}/.app.MainActivity")
    device("shell", "input", "keyevent", "KEYCODE_HOME")


with tempfile.TemporaryDirectory(prefix="cs-background-", dir="/tmp") as temporary:
    directory = pathlib.Path(temporary)
    log = (directory / "gateway.log").open("w+")
    env = dict(os.environ, CODEX_START_ANDROID_FIXTURE=temporary)
    fixture = subprocess.Popen(["cargo", "+1.88.0", "test", "--locked", "-p", "codex-start", "--bin", "codex-start", "remote::server::tests::android_notification_fixture", "--", "--ignored", "--exact"], cwd=ROOT, env=env, stdout=log, stderr=log)
    port, prepared, idle = None, False, False
    try:
        wait_for(lambda: (directory / "connection.json").exists(), timeout=180)
        data = json.loads((directory / "connection.json").read_text())
        port = data["port"]
        device("reverse", f"tcp:{port}", f"tcp:{port}")
        device("shell", "run-as", package, "mkdir", "-p", "files")
        write_fixture(data)
        device("shell", "pm", "grant", package, "android.permission.POST_NOTIFICATIONS")
        prepared = True
        instrument("prepare")
        # Keep the original monitoring preference written by the Android helper.
        data = json.loads(device("shell", "run-as", package, "cat", fixture_path).stdout)

        def status():
            return json.loads((directory / "status.json").read_text())

        def publish(number, event):
            staging = directory / "event.new"
            staging.write_text(json.dumps(event))
            staging.replace(directory / "event.json")
            wait_for(lambda: status()["sequence"] == number)

        def emit(number, kind="completed"):
            publish(number, {"method": "turn/completed", "params": {"threadId": "background-test", "turn": {"id": f"turn-{number}", "status": kind}}})

        def connected(timeout=60):
            wait_for(lambda: status()["connections"] > 0, timeout)
            wait_for(lambda: "CODEX_START_MONITOR:" in device("shell", "dumpsys", "activity", "service", f"{package}/.app.MonitorService").stdout.decode())

        def check(number=None, channel="complete", forbidden=(), request_id=None, forbidden_requests=()):
            # Read the running service without starting instrumentation, which can
            # change application and notification state on some Android systems.
            snapshot = device("shell", "dumpsys", "activity", "service", f"{package}/.app.MonitorService").stdout.decode()
            record = next(line.split("CODEX_START_MONITOR:", 1)[1] for line in snapshot.splitlines() if "CODEX_START_MONITOR:" in line)
            observed = json.loads(record)
            assert observed["enabled"], "Android notifications are disabled for the app"
            notifications = observed["notifications"]

            def notification_id(sequence):
                value = 0
                for character in f"{data['daemonId']}/{sequence}":
                    value = (31 * value + ord(character)) & 0xffffffff
                return value if value < 0x80000000 else value - 0x100000000

            def request_tag(value):
                return f"{data['daemonId']}/{data['sessionId']}/{json.dumps(value, separators=(',', ':'))}"

            if number is not None:
                matches = [item for item in notifications if (item["id"] == 2 and item.get("tag") == request_tag(request_id))] if request_id is not None else [item for item in notifications if item["id"] == notification_id(number)]
                assert len(matches) == 1, f"expected one notification for event {number}, observed {observed}"
                assert matches[0]["channel"] == channel
            assert not any(item["id"] == notification_id(sequence) for item in notifications for sequence in forbidden), "acknowledged event alerted again"
            assert not any(item.get("tag") == request_tag(value) for item in notifications for value in forbidden_requests), "resolved approval notification remains visible"

        foreground_then_home(); connected()
        if not args.doze_only:
            publish(1, {"id": 42, "method": "item/commandExecution/requestApproval", "params": {"threadId": "background-test", "command": "printf test"}})
            time.sleep(8); check(1, "input", request_id=42)
            publish(2, {"method": "serverRequest/resolved", "params": {"threadId": "background-test", "requestId": 42}})
            time.sleep(8); check(forbidden_requests=(42,))
            print("PASS required-input notification and dismissal after host resolution", flush=True)

            emit(3); time.sleep(8); check(3)
            print("PASS notification while the activity is in the background", flush=True)

            # Android cancels notifications on force-stop. The acknowledged event must
            # not return, while the new event must arrive after the user opens the app.
            device("shell", "am", "force-stop", package)
            wait_for(lambda: status()["connections"] == 0)
            emit(4)
            time.sleep(2)
            assert not device("shell", f"pidof {package} || true").stdout.strip(), "force-stop must prevent automatic restart"
            foreground_then_home(); connected(); time.sleep(8); check(4, forbidden=(3,))
            print("PASS force-stop recovery and notification deduplication", flush=True)

            foreground_then_home(); connected()
            pid = device("shell", "pidof", package).stdout.decode().strip()
            assert pid.isdigit(), "expected one application process"
            device("shell", "run-as", package, "kill", "-9", pid)
            wait_for(lambda: status()["connections"] == 0)
            emit(5, "failed")
            connected(180); time.sleep(8); check(5, "failure")
            print("PASS foreground service recovery after process death", flush=True)

        if args.doze or args.doze_only:
            foreground_then_home(); connected()
            device("shell", "dumpsys", "battery", "unplug")
            idle = True
            device("shell", "dumpsys", "deviceidle", "force-idle")
            assert device("shell", "dumpsys", "deviceidle", "get", "deep").stdout.decode().strip() == "IDLE", "Android did not enter deep idle"
            number = 1 if args.doze_only else 6
            emit(number); time.sleep(8)
            device("shell", "dumpsys", "deviceidle", "unforce")
            device("shell", "dumpsys", "battery", "reset")
            idle = False
            connected(); time.sleep(8); check(number)
            print("PASS event recovery after forced Doze", flush=True)
    finally:
        if idle:
            subprocess.run(adb + ["shell", "dumpsys", "deviceidle", "unforce"], capture_output=True, timeout=30)
            subprocess.run(adb + ["shell", "dumpsys", "battery", "reset"], capture_output=True, timeout=30)
        if prepared:
            try:
                instrument("cleanup")
            except Exception as error:
                print(f"Cleanup requires attention: {error}")
        subprocess.run(adb + ["shell", "run-as", package, "rm", "-f", fixture_path], capture_output=True, timeout=30)
        if port:
            subprocess.run(adb + ["reverse", "--remove", f"tcp:{port}"], capture_output=True, timeout=30)
        (directory / "stop").touch()
        try:
            fixture.wait(timeout=10)
        except subprocess.TimeoutExpired:
            fixture.terminate(); fixture.wait(timeout=10)
        if fixture.returncode:
            log.seek(0); print(log.read()[-5000:])
        log.close()
