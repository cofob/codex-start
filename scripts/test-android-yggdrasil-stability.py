#!/usr/bin/env python3
"""Measure two Android Yggdrasil groups, idle keepalive, and daemon restart recovery.

Runs the app repository's automatic reconnect loop. Uses isolated daemon state,
does not send model requests, and writes a report without invitation secrets.
"""
import argparse
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--serial", required=True)
parser.add_argument("--duration", type=int, default=600, help="seconds after initial connection, 360–3600")
parser.add_argument("--report", type=pathlib.Path, required=True)
parser.add_argument("--require-lan", action="store_true", help="require multicast peering with both host identities")
args = parser.parse_args()
if not 360 <= args.duration <= 3600:
    parser.error("duration must be 360–3600 seconds")
sdk = pathlib.Path(os.environ.get("ANDROID_SDK_ROOT", pathlib.Path.home() / "Library/Android/sdk"))
adb = [str(sdk / "platform-tools/adb"), "-s", args.serial]
package = "wtf.fob.cs"


def device(*command, **kwargs):
    return subprocess.run(adb + list(command), capture_output=True, check=True, timeout=30, **kwargs)


with tempfile.TemporaryDirectory(prefix="cs-stability-", dir="/tmp") as temporary:
    directory = pathlib.Path(temporary)
    # A concurrent build cannot replace the binary used by this test.
    binary = directory / "codex-start"
    shutil.copy2(ROOT / "target/release/codex-start", binary)
    environments, invitations = [], []
    runner = None
    report = {}
    restart_at = max(150, args.duration // 3)
    restarted = False
    stopped = False
    outage_started = None

    def call(env, *command):
        result = subprocess.run([str(binary), "--output", "json", *command], env=env, capture_output=True, text=True, timeout=30)
        if result.returncode:
            raise RuntimeError(result.stderr)
        return json.loads(result.stdout)

    try:
        for number in range(2):
            root = directory / str(number)
            env = dict(os.environ, XDG_DATA_HOME=str(root / "d"), XDG_CONFIG_HOME=str(root / "c"), XDG_CACHE_HOME=str(root / "cache"))
            env.pop("CODEX_START_CONFIG", None)
            environments.append(env)
            call(env, "daemon", "start", "--bind", "127.0.0.1:0")
            deadline = time.monotonic() + 90
            while not call(env, "daemon", "status").get("yggdrasil", "").startswith("connected:"):
                if time.monotonic() > deadline:
                    raise RuntimeError("Yggdrasil daemon did not become ready")
                time.sleep(1)
            invitations.append(call(env, "connect")["uri"])
        device("shell", "run-as", package, "mkdir", "-p", "files")
        device("shell", "run-as", package, "rm", "-f", "files/remote-stability-report.json")
        device("shell", f"run-as {package} sh -c 'cat > files/remote-stability.json'", input=json.dumps({"invitations": invitations, "durationSeconds": args.duration, "requireLan": args.require_lan}).encode())
        with (directory / "instrumentation.log").open("w+") as log:
            runner = subprocess.Popen(adb + ["shell", "am", "instrument", "-w", "-r", "-e", "class", "wtf.fob.cs.data.YggdrasilStabilityTest", f"{package}.test/androidx.test.runner.AndroidJUnitRunner"], stdout=log, stderr=log)
            deadline = time.monotonic() + args.duration + 300
            progress = -1
            while runner.poll() is None:
                if time.monotonic() > deadline:
                    raise RuntimeError("Android stability test timed out")
                result = subprocess.run(adb + ["shell", "run-as", package, "cat", "files/remote-stability-report.json"], capture_output=True, text=True, timeout=30)
                if result.returncode == 0:
                    report = json.loads(result.stdout)
                    elapsed = report["elapsedSeconds"]
                    if elapsed // 60 != progress:
                        progress = elapsed // 60
                        samples = report["samples"]
                        print(f"{elapsed}s: {sum(s['ok'] for s in samples)} successful probes; {sum(not s['ok'] for s in samples)} unavailable probes", flush=True)
                    if elapsed >= restart_at and not stopped:
                        outage_started = elapsed
                        call(environments[0], "daemon", "stop")
                        stopped = True
                        print("Stopped the first test daemon for 15 seconds", flush=True)
                    if elapsed >= restart_at + 15 and stopped and not restarted:
                        call(environments[0], "daemon", "restart")
                        restarted = True
                        print("Restarted the first test daemon with saved settings and identity", flush=True)
                time.sleep(2)
            log.seek(0)
            output = log.read()
            print(output)
            result = subprocess.run(adb + ["shell", "run-as", package, "cat", "files/remote-stability-report.json"], capture_output=True, text=True, timeout=30)
            if result.returncode == 0:
                report = json.loads(result.stdout)
            report["plannedRestartSecond"] = restart_at
            report["plannedOutageSeconds"] = 15
            report["outageStartedSecond"] = outage_started
            report["serial"] = args.serial
            report["requireLan"] = args.require_lan
            report["passed"] = runner.returncode == 0 and "OK (1 test)" in output and restarted
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2) + "\n")
            if not report["passed"]:
                raise RuntimeError("Yggdrasil stability acceptance failed; inspect the report")
    finally:
        if runner and runner.poll() is None:
            runner.terminate()
            runner.wait(timeout=10)
        if report:
            report.setdefault("passed", False)
            report.setdefault("plannedRestartSecond", restart_at)
            report.setdefault("plannedOutageSeconds", 15)
            report.setdefault("outageStartedSecond", outage_started)
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2) + "\n")
        subprocess.run(adb + ["shell", "run-as", package, "rm", "-f", "files/remote-stability.json", "files/remote-stability-report.json", "files/remote-stability-report.json.new"], capture_output=True, timeout=30)
        for env in environments:
            try:
                call(env, "daemon", "stop")
            except (RuntimeError, subprocess.TimeoutExpired):
                pass
