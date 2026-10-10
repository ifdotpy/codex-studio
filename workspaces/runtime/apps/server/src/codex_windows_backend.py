"""Start the Studio API with the selected Windows Python environment."""
from __future__ import annotations

from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))

from codex_canvas import main


if __name__ == "__main__":
    main()
