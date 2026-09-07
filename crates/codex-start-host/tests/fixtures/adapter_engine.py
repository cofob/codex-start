#!/usr/bin/env python3
"""Docker CLI and app-server fixture. No containers or model calls are made."""
import atexit
import base64
import csv
import json
import os
from pathlib import Path
import shutil
import sys

args = sys.argv[1:]
native = "app-server" in args
container_fs = os.environ.get("MOCK_CONTAINER_FS")


def container_path(path):
    path = Path(path)
    assert path.is_absolute()
    if not container_fs:
        return path
    return Path(container_fs).joinpath(*path.parts[1:])


if args[0] == "version":
    print("29.0.0")
    sys.exit()
if args[0] == "info":
    print('{"SecurityOptions":[]}')
    sys.exit()
if "--help" in args:
    print("--add-host --cap-add --cap-drop --label --mount --network --network-alias --read-only --security-opt --userns --internal --alias --filter --format")
    sys.exit()
if args[:2] == ["image", "inspect"]:
    sys.exit()
if args[:2] in (["volume", "inspect"], ["network", "inspect"], ["inspect", "--format"]):
    sys.exit(1)
if args[:2] in (["volume", "create"], ["network", "create"], ["network", "rm"]) or args[0] == "stop":
    sys.exit()
if args[0] == "exec":
    if os.environ.get("MOCK_COPIES"):
        with open(os.environ["MOCK_COPIES"], "a") as log:
            log.write(json.dumps(args) + "\n")
    if container_fs:
        command = args[4]
        if command == "test":
            target = container_path(args[-1])
            exists = target.exists() if args[-2] == "-e" else target.is_symlink()
            sys.exit(1 if exists else 0)
        if command == "mkdir":
            container_path(args[-1]).mkdir(parents=True, exist_ok=True)
        elif command != "chmod":
            sys.exit(99)
    sys.exit()
if args[0] == "cp":
    if os.environ.get("MOCK_COPIES"):
        with open(os.environ["MOCK_COPIES"], "a") as log:
            log.write(json.dumps(args) + "\n")
    if container_fs:
        source = Path(args[1])
        destination = container_path(args[2].split(":", 1)[1])
        if source.is_dir():
            shutil.copytree(source, destination)
        else:
            shutil.copy2(source, destination)
    sys.exit()
if args[0] == "top":
    activity = os.environ.get("MOCK_ACTIVITY")
    command = Path(activity).read_text().strip() if activity and Path(activity).exists() else ""
    if command == "error":
        print("mock process inspection failed", file=sys.stderr)
        sys.exit(1)
    print("PID PPID STAT COMMAND")
    print("100 1 S {MainThread} node /home/codex/.local/bin/codex app-server")
    print("101 100 S /opt/openai/codex app-server")
    print("102 100 S /usr/local/bin/codex-start-init unix-bridge --listen /tmp/agent.sock")
    if command:
        print(f"200 100 S {command}")
    sys.exit()
if not native and args[0] != "run":
    sys.exit(99)

if native:
    cwd = os.getcwd()
else:
    spec = None
    for i, arg in enumerate(args):
        if arg == "--mount":
            fields = dict(field.split("=", 1) for field in next(csv.reader([args[i + 1]])) if "=" in field)
            if fields.get("dst") == "/run/codex-start/init":
                spec = json.loads((Path(fields["src"]) / "spec.json").read_text())
    assert spec is not None
    cwd = spec["cwd"]
    if '"--version"' in json.dumps(spec["command"]):
        if os.environ.get("MOCK_PROBE_CWD"):
            Path(os.environ["MOCK_PROBE_CWD"]).write_text(cwd)
        print("codex-cli fixture")
        sys.exit()
if os.environ.get("MOCK_BACKENDS"):
    def record(event):
        with open(os.environ["MOCK_BACKENDS"], "a") as log:
            log.write(json.dumps({
                "event": event,
                "cwd": cwd,
                "codex_home": os.environ.get("CODEX_HOME"),
                "sqlite_home": os.environ.get("CODEX_SQLITE_HOME"),
            }) + "\n")
    start = "host-start" if native else "start"
    stop = "host-stop" if native else "stop"
    record(start)
    atexit.register(record, stop)
ready = False


def send(message):
    print(json.dumps(message), flush=True)


for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method is None:
        # Every backend uses the same server-originated ID to test remapping.
        assert request["id"] == 1
        send({"method": "test/approved", "params": {"cwd": cwd, "answer": request["result"]}})
        continue
    if method == "initialized":
        ready = True
        continue
    if method == "initialize":
        assert not ready
        send({"id": request["id"], "result": {"userAgent": "fixture", "cwd": cwd}})
        continue
    assert ready, "request sent before initialized"
    params = request.get("params", {})
    for requested_cwd in ([params["cwd"]] if params.get("cwd") else []) + params.get("cwds", []):
        assert str(Path(requested_cwd).resolve()) == requested_cwd, "cwd alias is not mounted"
    if method == "thread/read":
        result = {"thread": {"id": params["threadId"], "cwd": os.environ["MOCK_SAVED_CWD"]}}
    elif method in ("thread/start", "thread/resume"):
        if (
            method == "thread/resume"
            and not native
            and Path(cwd) == Path(os.environ["MOCK_SAVED_CWD"])
            and os.environ.get("MOCK_RESUME_ATTACHMENTS")
        ):
            for path in json.loads(os.environ["MOCK_RESUME_ATTACHMENTS"]):
                assert container_path(path).exists(), f"restored attachment is missing: {path}"
        thread_cwd = params.get("cwd", cwd)
        if method == "thread/resume" and not params.get("cwd"):
            thread_cwd = os.environ["MOCK_SAVED_CWD"]
        result = {"thread": {"id": params.get("threadId", thread_cwd), "cwd": thread_cwd}}
    elif method == "skills/list":
        requested = params.get("cwds", [cwd]) if native else [cwd]
        skill = str(Path(os.environ.get("CODEX_HOME", "/home/codex/.codex")) / "skills" / "fixture" / "SKILL.md")
        result = {"data": [{"cwd": path, "skills": [{"name": "fixture", "path": skill}]} for path in requested]}
    elif method == "test/approval":
        send({"id": 1, "method": "item/commandExecution/requestApproval", "params": {"cwd": cwd}})
        result = {}
    else:
        result = {"cwd": cwd, "params": params}
        attachments = []
        directories = []
        for item in params.get("input", []):
            path = Path(item.get("path", ""))
            if not path.is_absolute() or path == Path(cwd) or Path(cwd) in path.parents:
                continue
            copied = container_path(path)
            if copied.is_file():
                attachments.append(base64.b64encode(copied.read_bytes()).decode())
            elif copied.is_dir():
                directories.append(sorted(str(child.relative_to(copied)) for child in copied.rglob("*")))
        if attachments:
            result["attachmentDataBase64"] = attachments
        if directories:
            result["attachmentDirectories"] = directories
    send({"id": request["id"], "result": result})
