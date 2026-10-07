"""Own a helper deadline and output bound independently of the guest service."""
import os
import selectors
import signal
import subprocess
import sys
import time

LIMIT = 2 * 1024 * 1024
input_data = sys.stdin.buffer.read(LIMIT + 1)
if len(input_data) > LIMIT:
    sys.exit(125)
process = subprocess.Popen(sys.argv[2:], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                           stderr=subprocess.DEVNULL, start_new_session=True)
selector = selectors.DefaultSelector()
os.set_blocking(process.stdin.fileno(), False)
os.set_blocking(process.stdout.fileno(), False)
selector.register(process.stdin, selectors.EVENT_WRITE, "input")
selector.register(process.stdout, selectors.EVENT_READ, "output")
deadline = time.monotonic() + float(sys.argv[1])
offset = 0
output = bytearray()
code = None
try:
    while selector.get_map():
        if time.monotonic() >= deadline:
            code = 124
            break
        for key, _ in selector.select(min(0.1, max(0, deadline - time.monotonic()))):
            if key.data == "input":
                try:
                    offset += os.write(key.fd, input_data[offset:offset + 65536]) if offset < len(input_data) else 0
                except BrokenPipeError:
                    offset = len(input_data)
                if offset == len(input_data):
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
            else:
                block = os.read(key.fd, 65536)
                if not block:
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
                else:
                    output.extend(block)
                    if len(output) > LIMIT:
                        code = 125
                        break
        if code is not None:
            break
    if code is None:
        code = process.wait(timeout=max(0.01, deadline - time.monotonic()))
except subprocess.TimeoutExpired:
    code = 124
finally:
    selector.close()
    if process.poll() is None or code in {124, 125}:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=5)
if code not in {124, 125}:
    sys.stdout.buffer.write(output)
sys.exit(code)
