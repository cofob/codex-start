#!/usr/bin/env python3
"""Exercise the Android native client against two isolated local daemon processes."""
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
parser.add_argument("--serial", required=True, help="adb device serial")
parser.add_argument("--report", type=pathlib.Path, help="save connection timings without credentials")
parser.add_argument("--direct", action="store_true", help="use emulator host alias 10.0.2.2 instead of Yggdrasil")
args = parser.parse_args()
sdk = pathlib.Path(os.environ.get("ANDROID_SDK_ROOT", pathlib.Path.home() / "Library/Android/sdk"))
adb = [str(sdk / "platform-tools/adb"), "-s", args.serial]
binary = ROOT / "target/release/codex-start"
fixture = "files/remote-smoke-invitations.json"


def call(env, *argv):
    result = subprocess.run([str(binary), "--output", "json", *argv], env=env, capture_output=True, text=True, timeout=20)
    if result.returncode:
        raise RuntimeError(result.stderr)
    return json.loads(result.stdout)


with tempfile.TemporaryDirectory(prefix="cs-android-", dir="/tmp") as directory:
    stable_binary = pathlib.Path(directory) / "codex-start"
    shutil.copy2(binary, stable_binary)
    binary = stable_binary
    environments, invitations = [], []
    try:
        for number in range(2):
            root = pathlib.Path(directory) / str(number)
            env = dict(os.environ, XDG_DATA_HOME=str(root / "d"), XDG_CONFIG_HOME=str(root / "c"), XDG_CACHE_HOME=str(root / "cache"))
            env.pop("CODEX_START_CONFIG", None)
            environments.append(env)
            command = ["daemon", "start", "--bind", "127.0.0.1:0"]
            if args.direct:
                command.append("--no-yggdrasil")
            call(env, *command)
            deadline = time.monotonic() + 60
            while not args.direct:
                status = call(env, "daemon", "status")
                if status.get("yggdrasil", "").startswith("connected:"):
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError(f"Yggdrasil did not become ready: {status.get('yggdrasil')}")
                time.sleep(1)
            connect = ["connect"]
            if args.direct:
                connect += ["--host", "10.0.2.2"]
            uri = call(env, *connect)["uri"]
            invitations.append(uri)
        # Use stdin to keep invitation secrets out of shell arguments and test logs.
        subprocess.run(adb + ["shell", "run-as", "wtf.fob.cs", "mkdir", "-p", "files"], check=True)
        subprocess.run(adb + ["shell", f"run-as wtf.fob.cs sh -c 'cat > {fixture}'"], input=json.dumps(invitations).encode(), check=True)
        result = subprocess.run(adb + ["shell", "am", "instrument", "-w", "-r", "-e", "class", "wtf.fob.cs.data.RemoteConnectionTest", "wtf.fob.cs.test/androidx.test.runner.AndroidJUnitRunner"], capture_output=True, text=True, timeout=300)
        print(result.stdout)
        if result.returncode or "OK (1 test)" not in result.stdout:
            raise RuntimeError("Android remote transport test failed")
        timing = subprocess.run(adb + ["exec-out", "run-as", "wtf.fob.cs", "cat", "files/remote-smoke-timing.json"], capture_output=True, text=True, timeout=30)
        if timing.returncode == 0:
            measurements = json.loads(timing.stdout)
            print(json.dumps(measurements, indent=2))
            if args.report:
                args.report.write_text(json.dumps(measurements, indent=2) + "\n")
    finally:
        subprocess.run(adb + ["shell", f"run-as wtf.fob.cs rm -f {fixture}"], capture_output=True)
        for env in environments:
            try:
                call(env, "daemon", "stop")
            except (RuntimeError, subprocess.TimeoutExpired):
                pass
