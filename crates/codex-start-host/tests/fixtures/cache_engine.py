#!/usr/bin/env python3
"""Engine fixture for cache cleanup. Reject unexpected or destructive commands."""
import json
import os
from pathlib import Path
import re
import sys

args = sys.argv[1:]
path = Path(os.environ["CACHE_TEST_STATE"])
state = json.loads(path.read_text())
state.setdefault("calls", []).append(args)
path.write_text(json.dumps(state))

if args[0] == "version":
    print("29.0.0")
elif "--help" in args:
    print("--add-host --cap-add --cap-drop --label --mount --network --network-alias --read-only --security-opt --userns --internal --alias --filter --format")
elif args[:2] == ["volume", "ls"]:
    label = args[args.index("--filter") + 1].removeprefix("label=")
    key, value = label.split("=", 1)
    for name, volume in state["volumes"].items():
        if volume["labels"].get(key) == value or volume.get("stale_listing"):
            print(name)
elif args[:2] == ["volume", "inspect"]:
    volume = state["volumes"].get(args[-1])
    if not volume or volume.get("inspect_error"):
        sys.exit(1)
    key = re.search(r'index .Labels "([^"]+)"', args[args.index("--format") + 1])[1]
    print(volume["labels"].get(key, ""))
elif args[0] == "ps":
    assert "--all" in args, "stopped containers must be checked"
    name = args[args.index("--filter") + 1].removeprefix("volume=")
    volume = state["volumes"][name]
    if volume.get("probe_error"):
        print("container inspection failed", file=sys.stderr)
        sys.exit(1)
    for container in volume.get("containers", []):
        print(container)
elif args[:2] == ["volume", "rm"]:
    assert len(args) == 3 and not args[2].startswith("-"), "force removal is forbidden"
    name = args[2]
    volume = state["volumes"][name]
    assert not volume.get("containers"), "in-use volume must be skipped"
    if volume.get("remove_error"):
        print("volume is in use after probe", file=sys.stderr)
        sys.exit(1)
    del state["volumes"][name]
    path.write_text(json.dumps(state))
else:
    raise AssertionError(f"unexpected engine command: {args}")
