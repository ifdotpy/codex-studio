"""Read native account model metadata without touching active app-server sessions."""
from __future__ import annotations

import concurrent.futures
import json
import os
import queue
import selectors
import signal
import subprocess
import threading
import time


MAX_FRAME_BYTES = 2 * 1024 * 1024
MAX_RESPONSE_BYTES = 16 * 1024 * 1024


def read_model_catalog(home, *, executable, isolated, current=lambda: True, timeout=30):
    """Use native auth only. No thread, turn, command, or approval methods run.

    The caller owns one bounded worker. Stderr is drained and discarded, stdout
    has size limits, and only this reader's exact child process is terminated.
    """
    if not current():
        raise RuntimeError("Model catalog connection changed")
    env = os.environ.copy()
    env["CODEX_HOME"] = str(home)
    command = [executable, "app-server", "--listen", "stdio://"]
    if isolated:
        env.pop("OPENAI_API_KEY", None)
        env.pop("CODEX_API_KEY", None)
        command.extend(["-c", 'cli_auth_credentials_store="file"'])
    deadline = time.monotonic() + timeout
    job = None
    creationflags = 0
    if os.name == "nt":
        from codex_windows_supervisor import CREATE_SUSPENDED, assign_process, create_job, resume_process

        job = create_job()
        creationflags = CREATE_SUSPENDED
    try:
        proc = subprocess.Popen(command, env=env, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                start_new_session=os.name != "nt", creationflags=creationflags)
        if job is not None:
            try:
                assign_process(proc, job)
                resume_process(proc)
            except BaseException:
                job.terminate()
                job.close()
                proc.kill()
                proc.wait(timeout=2)
                raise
    except BaseException:
        if job is not None and job.handle:
            job.close()
        raise
    windows_readers = os.name == "nt"
    selector = None
    output_chunks: queue.Queue[tuple[str, bytes | None]] | None = None
    stop_readers = threading.Event()
    reader_overflow = threading.Event()
    windows_threads: list[threading.Thread] = []
    writer_threads: list[threading.Thread] = []
    try:
        if windows_readers:
            output_chunks = queue.Queue(maxsize=64)

            def drain_pipe(stream, name, emit):
                while not stop_readers.is_set():
                    try:
                        data = os.read(stream.fileno(), 65536)
                    except OSError:
                        data = b""
                    if not data:
                        if emit and not stop_readers.is_set():
                            try:
                                output_chunks.put_nowait((name, None))
                            except queue.Full:
                                reader_overflow.set()
                        return
                    if emit:
                        try:
                            output_chunks.put_nowait((name, data))
                        except queue.Full:
                            reader_overflow.set()
                            return

            for stream, name, emit in ((proc.stdout, "stdout", True),
                                       (proc.stderr, "stderr", False)):
                thread = threading.Thread(target=drain_pipe, args=(stream, name, emit),
                                          name="native-catalog-" + name, daemon=True)
                windows_threads.append(thread)
                thread.start()
        else:
            selector = selectors.DefaultSelector()
            selector.register(proc.stdout, selectors.EVENT_READ, "stdout")
            selector.register(proc.stderr, selectors.EVENT_READ, "stderr")
            os.set_blocking(proc.stdin.fileno(), False)
        buffer = bytearray()
        total_bytes = 0
        sequence = 0

        def send(message):
            data = json.dumps(message).encode("utf-8") + b"\n"
            # All requests are small metadata frames. A partial write is a
            # transport failure; this isolated process is never reused.
            if windows_readers:
                result: list[BaseException] = []
                completed = threading.Event()

                def write_frame() -> None:
                    try:
                        proc.stdin.write(data)
                        proc.stdin.flush()
                    except BaseException as error:
                        result.append(error)
                    finally:
                        completed.set()

                thread = threading.Thread(target=write_frame, name="native-catalog-stdin", daemon=True)
                writer_threads.append(thread)
                thread.start()
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not completed.wait(remaining):
                    raise TimeoutError("Native model catalog timed out")
                if result:
                    raise RuntimeError("Native model catalog input failed") from result[0]
            elif os.write(proc.stdin.fileno(), data) != len(data):
                raise RuntimeError("Native model catalog input did not drain")

        def request(method, params):
            nonlocal sequence, total_bytes
            sequence += 1
            send({"id": sequence, "method": method, "params": params})
            while True:
                if not current():
                    raise RuntimeError("Model catalog connection changed")
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("Native model catalog timed out")
                while b"\n" in buffer:
                    line, _, tail = buffer.partition(b"\n")
                    buffer[:] = tail
                    if not line.strip():
                        continue
                    message = json.loads(line)
                    if not isinstance(message, dict):
                        raise ValueError("Invalid native model catalog message")
                    if "method" in message:
                        if "id" in message:
                            raise RuntimeError("Native model catalog requested an unsupported action")
                        continue
                    if message.get("id") != sequence:
                        raise ValueError("Unexpected native model catalog response identity")
                    if "error" in message:
                        # Native errors can include account details. Do not
                        # publish raw response or stderr text to the UI.
                        raise RuntimeError(f"Native {method} failed")
                    if "result" not in message:
                        raise ValueError("Invalid native model catalog response")
                    return message["result"]
                if windows_readers:
                    assert output_chunks is not None
                    if reader_overflow.is_set():
                        raise ValueError("Native model catalog output exceeded its read buffer")
                    try:
                        stream_name, data = output_chunks.get(timeout=min(remaining, .25))
                    except queue.Empty:
                        continue
                    if data is None:
                        if stream_name == "stdout":
                            raise RuntimeError("Native model catalog closed its output")
                        continue
                    if stream_name == "stdout":
                        total_bytes += len(data)
                        buffer.extend(data)
                        if len(buffer) > MAX_FRAME_BYTES or total_bytes > MAX_RESPONSE_BYTES:
                            raise ValueError("Native model catalog response is too large")
                else:
                    assert selector is not None
                    events = selector.select(min(remaining, .25))
                    for key, _ in events:
                        data = os.read(key.fileobj.fileno(), 65536)
                        if not data:
                            selector.unregister(key.fileobj)
                            if key.data == "stdout":
                                raise RuntimeError("Native model catalog closed its output")
                            continue
                        if key.data == "stdout":
                            total_bytes += len(data)
                            buffer.extend(data)
                            if len(buffer) > MAX_FRAME_BYTES or total_bytes > MAX_RESPONSE_BYTES:
                                raise ValueError("Native model catalog response is too large")

        request("initialize", {"clientInfo": {"name": "codex_studio_catalog", "version": "1.0.0"},
                               "capabilities": {"experimentalApi": True}})
        send({"method": "initialized"})
        rows, cursors, models = [], set(), set()
        first_page = None
        cursor = None
        while True:
            value = request("model/list", {"limit": 100, "includeHidden": True,
                                           **({"cursor": cursor} if cursor is not None else {})})
            if not isinstance(value, dict) or not isinstance(value.get("data"), list):
                raise ValueError("Invalid model/list response")
            if first_page is None:
                first_page = value
            for row in value["data"]:
                if (not isinstance(row, dict) or not isinstance(row.get("model"), str)
                        or not row["model"].strip() or row["model"] in models):
                    raise ValueError("Invalid or duplicate model in model/list pages")
                models.add(row["model"])
                rows.append(row)
            cursor = value.get("nextCursor")
            if cursor is None:
                if not current():
                    raise RuntimeError("Model catalog connection changed")
                return {**first_page, "data": rows, "nextCursor": None}
            if (not isinstance(cursor, str) or not cursor or cursor in cursors
                    or len(cursors) >= 100):
                raise ValueError("Invalid or repeated model/list cursor")
            cursors.add(cursor)
    finally:
        if selector is not None:
            selector.close()
        stop_readers.set()
        if job is not None:
            try:
                job.terminate()
                job.wait_empty(timeout=1)
            finally:
                job.close()
        else:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=.5)
            except subprocess.TimeoutExpired:
                pass
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        try:
            proc.wait(timeout=1)
        except subprocess.TimeoutExpired:
            proc.kill()
            try:
                proc.wait(timeout=1)
            except subprocess.TimeoutExpired:
                pass
        for pipe in (proc.stdin, proc.stdout, proc.stderr):
            try:
                pipe.close()
            except OSError:
                pass
        for thread in (*windows_threads, *writer_threads):
            thread.join(timeout=.5)


def submit_model_catalog(home, *, executable, isolated, current):
    future = concurrent.futures.Future()

    def run():
        try:
            future.set_result(read_model_catalog(home, executable=executable, isolated=isolated, current=current))
        except Exception as error:
            future.set_exception(error)

    threading.Thread(target=run, name="native-model-catalog", daemon=True).start()
    return future
