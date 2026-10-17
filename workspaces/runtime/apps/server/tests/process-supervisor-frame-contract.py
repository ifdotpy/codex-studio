#!/usr/bin/env python3
"""Bounded supervisor JSON-lines framing on POSIX transports."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import json
import io
import os
import socket
import threading
import unittest
from types import SimpleNamespace

import codex_process_supervisor as supervisor


@unittest.skipUnless(os.name == "posix", "POSIX supervisor frame contract")
class ProcessSupervisorFrameContract(unittest.TestCase):
    def test_large_native_history_envelope_round_trips(self):
        payload = {"requestId": 7, "result": {"event": {
            "sequence": 1, "kind": "stdout", "generation": 1,
            "payload": json.dumps({"result": {"data": [
                {"text": 'native history "quoted"\n' * 100000}
            ]}}),
        }}}
        writer = io.BytesIO()
        supervisor._write_frame(writer, payload)
        self.assertGreater(len(writer.getvalue()), 1024 * 1024)
        self.assertEqual(supervisor._recv(None, io.StringIO(
            writer.getvalue().decode())), payload)

    def test_send_rejects_oversized_frame_before_writing(self):
        class Writer:
            def __init__(self):
                self.data = bytearray()

            def sendall(self, data):
                self.data.extend(data)

        writer = Writer()
        with self.assertRaisesRegex(ValueError, "frame exceeds the size limit"):
            supervisor._send(
                writer,
                {"payload": "x" * supervisor.MAX_FRAME_BYTES},
            )
        self.assertEqual(writer.data, b"")

    def test_client_rejects_oversized_posix_response_frame(self):
        sender, receiver = socket.socketpair()
        reader = receiver.makefile("r", encoding="utf-8")
        payload = b'{"payload":"' + b"x" * supervisor.MAX_FRAME_BYTES + b'"}\n'

        def send_payload():
            try:
                sender.sendall(payload)
            except OSError:
                pass

        thread = threading.Thread(target=send_payload)
        thread.start()
        try:
            with self.assertRaisesRegex(ValueError, "frame exceeds the size limit"):
                supervisor._recv(receiver, reader)
        finally:
            reader.close()
            receiver.close()
            sender.close()
            thread.join(timeout=2)
        self.assertFalse(thread.is_alive())

    def test_server_reads_only_a_bounded_oversized_posix_request(self):
        class Reader:
            requested_size = None

            def readline(self, size):
                self.requested_size = size
                return b"x" * size

        class Writer:
            data = b""

            def write(self, value):
                self.data += value

            def flush(self):
                pass

        reader = Reader()
        writer = Writer()
        server = SimpleNamespace(supervisor=SimpleNamespace(root=Path("/unused")))
        supervisor._serve_connection(server, reader, writer)

        self.assertEqual(reader.requested_size, supervisor.MAX_FRAME_BYTES + 1)
        self.assertIn("size limit", json.loads(writer.data)["error"])

    def test_oversized_posix_response_uses_the_existing_error_projection(self):
        class Writer:
            data = b""

            def write(self, value):
                self.data += value

            def flush(self):
                pass

        writer = Writer()
        supervisor._write_frame(
            writer,
            {"requestId": 8, "payload": "x" * supervisor.MAX_FRAME_BYTES},
        )
        self.assertEqual(
            json.loads(writer.data),
            {"requestId": 8, "error": "Supervisor response exceeds the size limit"},
        )


if __name__ == "__main__":
    unittest.main()
