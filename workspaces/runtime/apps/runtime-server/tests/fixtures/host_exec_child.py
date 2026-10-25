"""Own the manual fixture's exact service child until the parent closes its channel."""

import json
import os
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile


# Bound one kernel identity query; unit: seconds, manual fixture only.
PROBE_SECONDS = 2


def publish(path, value):
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as output:
        json.dump(value, output, sort_keys=True)
        output.flush()
        os.fsync(output.fileno())
        pending = output.name
    os.replace(pending, path)


def identity(child, argv):
    result = subprocess.run(
        ["ps", "-ww", "-p", str(child.pid), "-o", "uid=,lstart=,command="],
        check=True, capture_output=True, text=True, timeout=PROBE_SECONDS,
        env={**os.environ, "LC_ALL": "C"},
    ).stdout.strip().split(maxsplit=6)
    if len(result) != 7 or int(result[0]) != os.getuid() or result[6] != " ".join(argv):
        raise RuntimeError("the owned child's UID or exact command differs")
    executable = str(Path(argv[0]).resolve())
    images = subprocess.run(
        ["lsof", "-a", "-p", str(child.pid), "-d", "txt", "-Fn"],
        check=True, capture_output=True, text=True, timeout=PROBE_SECONDS,
    ).stdout.splitlines()
    if "n" + executable not in images:
        raise RuntimeError("the owned child's executable differs")
    return {"pid": child.pid, "uid": int(result[0]), "birth": " ".join(result[1:6]),
            "executable": executable, "argv": argv}


def run():
    owner_path, receipt, guard, program, *args = sys.argv[1:]
    receipt = Path(receipt)
    guard = float(guard)
    argv = [program, *args]
    # Connect before any spawn. A parent that already ended cannot leave a late child.
    owner = socket.socket(socket.AF_UNIX)
    owner.connect(owner_path)
    if select.select([owner], [], [], 0)[0] and not owner.recv(1):
        return 0
    child = subprocess.Popen(argv)
    proof = {"pid": child.pid, "uid": os.getuid(), "argv": argv,
             "executable": str(Path(program).resolve())}
    code = 1
    cleanup_allowed = False
    try:
        publish(receipt, {"phase": "running", "identity": proof})
        try:
            proof = identity(child, argv)
        except Exception as error:
            proof["auditError"] = str(error)
        publish(receipt, {"phase": "running", "identity": proof})
        while child.poll() is None:
            if select.select([owner], [], [], 0.05)[0]:
                try:
                    message = owner.recv(1)
                except ConnectionResetError:
                    message = b""
                # The parent permits service cleanup only after exact command cancellation.
                if message == b"C":
                    cleanup_allowed = True
                if cleanup_allowed or not message:
                    break
    except Exception as error:
        print("owned host_exec cleanup unproved: " + str(error), file=sys.stderr)
    finally:
        owner.close()
        try:
            if child.poll() is None:
                if not cleanup_allowed:
                    raise RuntimeError("the parent has not proved fixture command cleanup")
                # Only Popen signals its own unreaped child. Audit queries grant no authority.
                child.terminate()
                try:
                    child.wait(timeout=guard)
                except subprocess.TimeoutExpired:
                    child.kill()
            code = child.wait(timeout=guard)
            final = {"phase": "exited", "identity": proof, "code": code}
        except Exception as error:
            final = {"phase": "cleanup_unknown", "identity": proof,
                     "pid": child.pid, "argv": argv, "error": str(error)}
        try:
            publish(receipt, final)
        except OSError:
            print(json.dumps(final, sort_keys=True), file=sys.stderr)
    return code if code >= 0 else 128 - code


if __name__ == "__main__":
    raise SystemExit(run())
