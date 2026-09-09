"""Record the native PTY session identity, then replace this process with the shell."""
import os
from pathlib import Path
import sys

marker = Path(sys.argv[1])
with marker.with_suffix(".tmp").open("x") as output:
    os.chmod(output.name, 0o600)
    output.write(str(os.getpid()))
os.replace(output.name, marker)
os.execv(sys.argv[2], [sys.argv[2], "-l"])
