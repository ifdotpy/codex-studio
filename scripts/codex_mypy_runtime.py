"""Run the strict Python ratchet with Studio's managed development interpreter."""

import subprocess
import sys
from pathlib import Path

from codex_python import interpreter_has_api, managed_python

scripts = Path(__file__).parent.resolve()
python = managed_python(scripts, development=True)
if not python.is_file() or not interpreter_has_api(python):
    sys.exit(
        "The API typing environment is missing. Run `python3 scripts/install-cli.py --dev` during setup."
    )
raise SystemExit(
    subprocess.run(
        [
            str(python),
            "-m",
            "mypy",
            "--config-file",
            str(scripts.parent / "mypy.ini"),
            str(scripts),
        ],
        check=False,
    ).returncode
)
