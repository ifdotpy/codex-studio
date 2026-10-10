"""One bounded Ed25519 helper process per credential service."""
from __future__ import annotations

import json
from collections.abc import Iterator
from contextlib import contextmanager
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import threading
import time
from typing import Any, cast


class CryptoProcess:
    def __init__(self) -> None:
        self.lock = threading.RLock()
        self.process: subprocess.Popen[bytes] | None = None
        self.sequence = 0
        self.closed = False

    def shutdown(self) -> None:
        with self.lock:
            self.closed = True
            self.close()

    def close(self) -> None:
        with self.lock:
            process, self.process = self.process, None
            if process is None:
                return
            if process.poll() is None:
                process.kill()
            process.wait(timeout=2)
            for pipe in (process.stdin, process.stdout):
                if pipe is not None:
                    pipe.close()

    def __del__(self) -> None:
        try:
            self.close()
        except (OSError, subprocess.SubprocessError):
            pass

    @contextmanager
    def _deadline_lock(self, timeout: float) -> Iterator[float]:
        deadline = time.monotonic() + timeout
        if not self.lock.acquire(timeout=max(0, timeout)):
            raise RuntimeError("The Ed25519 service is busy")
        try:
            yield deadline
        finally:
            self.lock.release()

    def call(self, operation: str, *, timeout: float = 8, **fields: Any) -> dict[str, Any]:
        with self._deadline_lock(timeout) as deadline:
            try:
                if self.closed:
                    raise RuntimeError("The Ed25519 service is closed")
                node = os.environ.get("CODEX_NODE") or shutil.which("node")
                if not node:
                    raise OSError("The Node runtime is unavailable")
                self.sequence += 1
                request = json.dumps({"id": self.sequence, "operation": operation, **fields},
                                     separators=(",", ":")).encode() + b"\n"
                if len(request) > 1_048_576:
                    raise ValueError("The crypto request is too large")
                if os.name == "nt":
                    result = subprocess.run(
                        [node, str(Path(__file__).with_suffix(".mjs"))],
                        input=request, capture_output=True, timeout=max(0.1, deadline - time.monotonic()),
                        check=True, env={**os.environ, "ELECTRON_RUN_AS_NODE": "1"},
                    )
                    if len(result.stdout) > 1_048_576:
                        raise ValueError("The crypto response is too large")
                    response = json.loads(result.stdout)
                    if (not isinstance(response, dict) or response.get("id") != self.sequence
                            or not isinstance(response.get("result"), dict)):
                        raise ValueError("The crypto response is invalid")
                    return cast(dict[str, Any], response["result"])
                if self.process is None or self.process.poll() is not None:
                    self.close()
                    self.process = subprocess.Popen(
                        [node, str(Path(__file__).with_suffix(".mjs"))], stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                        env={**os.environ, "ELECTRON_RUN_AS_NODE": "1"},
                    )
                process = self.process
                assert process.stdin is not None and process.stdout is not None
                incoming = bytearray()
                offset = 0
                for pipe in (process.stdin, process.stdout):
                    os.set_blocking(pipe.fileno(), False)
                with selectors.DefaultSelector() as selector:
                    selector.register(process.stdin, selectors.EVENT_WRITE)
                    selector.register(process.stdout, selectors.EVENT_READ)
                    while b"\n" not in incoming:
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            raise TimeoutError
                        for selected, _ in selector.select(remaining):
                            if selected.fileobj is process.stdin:
                                offset += os.write(process.stdin.fileno(), request[offset:])
                                if offset == len(request):
                                    selector.unregister(process.stdin)
                            else:
                                chunk = os.read(process.stdout.fileno(), 65_536)
                                if not chunk:
                                    raise OSError("The crypto process stopped")
                                incoming.extend(chunk)
                                if len(incoming) > 1_048_576:
                                    raise ValueError("The crypto response is too large")
                result = json.loads(incoming)
                if (not isinstance(result, dict) or result.get("id") != self.sequence
                        or not isinstance(result.get("result"), dict)):
                    raise ValueError("The crypto response is invalid")
                return cast(dict[str, Any], result["result"])
            except (OSError, ValueError, subprocess.SubprocessError, TimeoutError):
                self.close()
                raise RuntimeError("The Ed25519 service is unavailable") from None
