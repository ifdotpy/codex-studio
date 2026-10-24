"""Test-only helper commands: real CLI/deadline processes with injected durability.

Never selected by Service's production command factory.
"""
from pathlib import Path
import sys


def worker_command(script):
    if script in {"upload.py", "upload_apply.py"}:
        return [sys.executable, str(Path(__file__)), script]
    return [sys.executable, str(Path(__file__).with_name(script))]


if __name__ == "__main__":
    script = sys.argv.pop(1)
    if script not in {"upload.py", "upload_apply.py"}:
        raise ValueError("unexpected test worker")
    module = __import__(Path(script).stem)
    if "--pause-after-chunk" in sys.argv:
        # Stop after writing a small real chunk, before the worker can finish extraction.
        # stdout is the readiness pipe; the parent kills us while we own extract.lock.
        sys.argv.remove("--pause-after-chunk")
        import tarfile
        read = tarfile.ExFileObject.read
        calls = 0

        def pause_read(source, count=-1):
            global calls
            calls += 1
            if calls == 2:
                print("chunk-written", flush=True)
                sys.stdin.buffer.read(1)
            return read(source, count)

        tarfile.ExFileObject.read = pause_read
    module.main(sync_tree=lambda: None)
