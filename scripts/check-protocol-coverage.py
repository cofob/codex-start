#!/usr/bin/env python3
"""Check that each vendored protocol method and TUI command has a coverage entry."""
import argparse
import json
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--release", action="store_true", help="also require all acceptance entries to pass")
args = parser.parse_args()
coverage = json.loads((ROOT / "protocol/coverage-map.json").read_text())
keys = [(e["kind"], e["method"]) for e in coverage["entries"]]
assert len(keys) == len(set(keys)), "duplicate coverage entry"
expected = set()
for kind in ("ClientRequest", "ServerRequest", "ServerNotification", "ClientNotification"):
    schema = json.loads((ROOT / f"protocol/codex/{kind}.json").read_text())
    expected.update((kind, variant["properties"]["method"]["enum"][0]) for variant in schema["oneOf"])
tui = json.loads((ROOT / "protocol/tui-commands.json").read_text())
assert tui["revision"] == coverage["baseline"], "baseline revisions differ"
expected.update(("TuiCommand", "/" + command) for command in tui["commands"])
expected.update(("DeprecatedRequest", name) for name in ("getAuthStatus", "getConversationSummary", "gitDiffToRemote"))
assert expected == set(keys), f"coverage mismatch: {expected.symmetric_difference(keys)}"
for entry in coverage["entries"]:
    assert entry["control"] and entry["implementation"] and entry["verification"], entry
if args.release:
    assert coverage["complete"], "feature acceptance is not complete; release is blocked"
    assert all("pending" not in entry["verification"] and entry["control"] != "pending" for entry in coverage["entries"]), "pending acceptance entries block release"
print(f"Coverage map contains all {len(keys)} baseline entries; acceptance complete: {coverage['complete']}")
