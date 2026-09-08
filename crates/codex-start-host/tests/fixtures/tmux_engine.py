#!/usr/bin/env python3
"""Engine fixture that records tmux probes and rejects container mutations."""
import json
import os
from pathlib import Path
import re
import sys

args = sys.argv[1:]
path = Path(os.environ["TMUX_TEST_STATE"])
state = json.loads(path.read_text())
state.setdefault("calls", []).append(args)
path.write_text(json.dumps(state))
if args[0] == "version":
    print("29.0.0")
elif "--help" in args:
    print("--add-host --cap-add --cap-drop --label --mount --network --network-alias --read-only --security-opt --userns --internal --alias --filter --format")
elif args[0] == "ps":
    for name, container in state["containers"].items():
        if container["running"]:
            print(json.dumps({"Names": name}))
elif args[:2] == ["container", "inspect"]:
    container = state["containers"].get(args[-1])
    if container is None:
        sys.exit(1)
    template = args[args.index("--format") + 1]
    if "State.Running" in template:
        print(str(container["running"]).lower())
    else:
        key = re.search(r'index .Config.Labels "([^"]+)"', template)[1]
        print(container["labels"].get(key, ""))
elif args[0] == "exec":
    assert args[args.index("--user") + 1] == "codex", "use the tmux server owner"
    position = args.index("tmux")
    name = args[position - 1]
    command = args[position:]
    assert command[:3] == ["tmux", "-L", "codex-start"]
    assert command[4:] == ["-t", "=codex"]
    container = state["containers"][name]
    if command[3] == "has-session":
        sys.exit(0 if container["tmux"] else 1)
    assert command[3] == "attach-session"
    assert "--tty" in args and "--interactive" in args
    assert "--env" in args and "TERM=screen-256color" in args
    print("attached " + name)
    sys.exit(container.get("attach_status", 0))
else:
    print("fresh launch reached: " + repr(args), file=sys.stderr)
    sys.exit(99)
