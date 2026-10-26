import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps" / "server" / "src"))
from codex_provider_update_notice import compare_versions, parse_version

cases = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
print(json.dumps([
    [
        parse_version(installed) is not None,
        parse_version(available) is not None,
        compare_versions(installed, available) is not None,
    ]
    for installed, available in cases
]))
