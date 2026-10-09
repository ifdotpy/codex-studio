"""Stable launch identity; mount namespace selection happens only at child launch."""
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
import codex_workspace_images as images

agent, cwd, *argv = sys.argv[1:]
images.ensure_mounted(agent)
prefix = images.exec_prefix()
command = [*prefix, sys.executable, "-c",
           "import os,sys; os.chdir(sys.argv[1]); os.execvpe(sys.argv[2],sys.argv[2:],os.environ)", cwd, *argv]
os.execvpe(command[0], command, os.environ)
