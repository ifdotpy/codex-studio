"""Profile normalization through the actual Python boundary."""
import json
import os
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).parent / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "apps/server/src"))
from codex_claude import profile_options
cases = [{}, {"unknown": 1}]
for field, values in {
    "autoCompactWindow": [None, "", "200000", 200000, True, 99999, 1000001, 200000.0],
    "launchArgs": [None, "", "--verbose", "--add-dir 'two words' --debug", "--verbose --verbose", "--resume", "'"],
    "customModels": [["fixture"], [{"id":"fixture","label":"  Label  "}], [{"id":"fixture","label":5}], ["bad id"], ["same","same"], None],
    "configDir": ["~/fixtures/../profile", "", None, "bad\u0000path"],
}.items():
    cases.extend({field: value} for value in values)
results = []
for options in cases:
    try:
        result = profile_options(options)
    except ValueError:
        result = None
    results.append({"options": options, "result": result, "home":os.environ["HOME"]})
print(json.dumps(results))
