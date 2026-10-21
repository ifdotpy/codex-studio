"""Keep current Rust-hosted delivery tools aligned with the Python tool owner.

Only these current tool entries are refreshed. Historical native-tools snapshots
and all other bundled preparation resources retain their existing contents.
"""
from __future__ import annotations

import argparse
import copy
import json
import subprocess
from typing import Any

from codex_layout import REPOSITORY_ROOT
from codex_runtime import TOOLS

# Current sender tools whose delivery API is owned by codex_runtime.TOOLS.
DELIVERY_TOOL_NAMES = ("orchestration_message", "orchestration_send")
BUNDLED_RESOURCES = REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-services/src/account_worker/preparation-resources.json"


def current_delivery_tools() -> dict[str, Any]:
    definitions = {definition["name"]: definition for definition in TOOLS
                   if definition["name"] in DELIVERY_TOOL_NAMES}
    if set(definitions) != set(DELIVERY_TOOL_NAMES):
        raise ValueError("The Python delivery tool owner is missing a declaration")
    return definitions


def stale_delivery_tools(resources: dict[str, Any]) -> list[str]:
    expected = current_delivery_tools()
    tools = resources["tools"]
    for name in DELIVERY_TOOL_NAMES:
        if sum(tool.get("name") == name for tool in tools) != 1:
            raise ValueError(f"Bundled preparation must contain exactly one {name} declaration")
    return [name for name in DELIVERY_TOOL_NAMES
            if next(tool for tool in tools if tool.get("name") == name) != expected[name]]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--write", action="store_true")
    options = parser.parse_args()
    resources = json.loads(BUNDLED_RESOURCES.read_text())
    stale = stale_delivery_tools(resources)
    if not stale:
        return 0
    if not options.write:
        for name in stale:
            print(f"{name} is stale in {BUNDLED_RESOURCES}; run generate_delivery_tools.py --write")
        return 1
    definitions = current_delivery_tools()
    resources["tools"] = [copy.deepcopy(definitions[tool["name"]])
                          if tool["name"] in DELIVERY_TOOL_NAMES else tool
                          for tool in resources["tools"]]
    BUNDLED_RESOURCES.write_text(json.dumps(resources, ensure_ascii=False, indent=2) + "\n")
    subprocess.run(["pnpm", "exec", "oxfmt", "--write", str(BUNDLED_RESOURCES)], cwd=REPOSITORY_ROOT, check=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
