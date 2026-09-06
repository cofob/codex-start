#!/usr/bin/env python3
"""Docker CLI and app-server fixture. No containers or model calls are made."""
import atexit
import csv
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
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
if args[0] != "run":
    sys.exit(99)

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
            log.write(json.dumps({"event": event, "cwd": cwd}) + "\n")
    record("start")
    atexit.register(record, "stop")
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
        result = {"thread": {"id": params.get("threadId", cwd), "cwd": cwd}}
    elif method == "skills/list":
        result = {"data": [{"cwd": cwd, "skills": []}]}
    elif method == "test/approval":
        send({"id": 1, "method": "item/commandExecution/requestApproval", "params": {"cwd": cwd}})
        result = {}
    else:
        result = {"cwd": cwd, "params": params}
    send({"id": request["id"], "result": result})
