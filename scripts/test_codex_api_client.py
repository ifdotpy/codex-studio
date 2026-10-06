"""Fault-server contracts for the narrow Studio API client and CLI readers."""

from contextlib import contextmanager, redirect_stderr, redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import runpy
import socket
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from typing import Any, Iterator
from urllib.parse import parse_qs, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))
import codex_api_client
from codex_canvas import identity


SCRIPT = Path(__file__).resolve().parent / "codex-control"


class FaultHandler(BaseHTTPRequestHandler):
    def do_GET(self: "Any") -> None:
        self.server.requests.append(("GET", self.path, None))
        self.server.headers.append(("GET", self.path, self.headers.get("X-Canvas-Token")))
        self.server.respond(self)

    def do_POST(self: "Any") -> None:
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.server.requests.append(("POST", self.path, json.loads(body)))
        self.server.headers.append(("POST", self.path, self.headers.get("X-Canvas-Token")))
        self.server.respond(self)

    def log_message(self: "Any", *_args: "Any") -> None:
        pass


@contextmanager
def fault_server(callback: "Any") -> "Iterator[tuple[str, Any, list[Any]]]":
    server: "Any" = ThreadingHTTPServer(("127.0.0.1", 0), FaultHandler)
    server.requests = []
    server.headers = []
    server.callback = callback

    def respond(handler: "Any") -> "Any":
        status, value, content_type = server.callback(handler.command, handler.path, handler)
        body = value if isinstance(value, bytes) else json.dumps(value).encode()
        handler.send_response(status)
        handler.send_header("Content-Type", content_type)
        handler.send_header("Content-Length", str(len(body)))
        handler.end_headers()
        handler.wfile.write(body)

    server.respond = respond
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    origin = f"http://127.0.0.1:{server.server_port}"
    original_connect = socket.socket.connect
    attempts = []

    def guarded_connect(sock: "Any", address: "Any") -> "Any":
        if (not isinstance(address, tuple) or len(address) < 2
                or address[0] not in ("127.0.0.1", "localhost")
                or address[1] != server.server_port):
            raise AssertionError(f"network escape blocked: {address!r}")
        attempts.append(address)
        return original_connect(sock, address)

    try:
        with patch.object(socket.socket, "connect", guarded_connect), patch.dict(os.environ, {
            "HTTP_PROXY": "http://127.0.0.1:9",
            "HTTPS_PROXY": "http://127.0.0.1:9",
            "http_proxy": "http://127.0.0.1:9",
            "https_proxy": "http://127.0.0.1:9",
            "NO_PROXY": "",
            "no_proxy": "",
        }):
            yield origin, server, attempts
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


def entity(collection: "Any", key: "Any", value: "Any", sequence: "Any", deleted: "Any"=False) -> "Any":
    return {
        "id": f"entity:{collection}:{key}",
        "payload": json.dumps({"collection": collection, "id": key, "value": value}),
        "seq": sequence,
        "_deleted": deleted,
    }


def page(documents: "Any", checkpoint: "Any", maximum: "Any") -> "Any":
    return {"documents": documents, "checkpoint": {"seq": checkpoint}, "maxSeq": maximum}


class CodexAPIClientContracts(unittest.TestCase):
    def test_session_and_desktop_helpers_use_their_narrow_endpoints(self: "Any") -> None:
        def route(_method: "Any", path: "Any", _handler: "Any") -> "Any":
            if path == "/api/session":
                return 200, {"token": "test-token"}, "application/json"
            if path == "/api/desktop":
                return 200, {"stateDir": "/scratch/studio-state"}, "application/json"
            return 404, {"error": "not found"}, "application/json"

        with fault_server(route) as (origin, server, attempts):
            self.assertEqual(codex_api_client.session_token(origin), "test-token")
            self.assertEqual(codex_api_client.desktop_state_dir(origin), "/scratch/studio-state")
            self.assertEqual([path for _, path, _ in server.requests], ["/api/session", "/api/desktop"])
            self.assertEqual(len(attempts), 2)

    def test_entity_pull_reconciles_later_versions_and_tombstones_until_max_seq(self: "Any") -> None:
        responses = {
            0: page([entity("agent", "a", {"id": "a", "revision": 1}, 1)], 1, 4),
            1: page([
                entity("agent", "a", {"id": "a", "revision": 2}, 2),
                entity("agent", "b", {"id": "b"}, 3),
            ], 3, 4),
            3: page([entity("agent", "b", {}, 4, deleted=True)], 4, 4),
        }

        def route(_method: "Any", path: "Any", _handler: "Any") -> "Any":
            query = parse_qs(urlsplit(path).query)
            self.assertEqual(query["scope"], [codex_api_client.ENTITY_SCOPE])
            return 200, responses[int(query["after"][0])], "application/json"

        with fault_server(route) as (origin, server, attempts):
            self.assertEqual(codex_api_client.pull_entities(origin, "test-token"), [
                {"collection": "agent", "id": "a", "value": {"id": "a", "revision": 2}}
            ])
            self.assertEqual(
                [parse_qs(urlsplit(path).query)["after"][0] for _, path, _ in server.requests],
                ["0", "1", "3"],
            )
            self.assertEqual(
                [token for _, path, token in server.headers if path.startswith("/api/sync/pull")],
                ["test-token", "test-token", "test-token"],
            )
            self.assertEqual(len(attempts), 3)

    def test_entity_pull_caps_unfinished_pagination_truthfully(self: "Any") -> None:
        def route(_method: "Any", path: "Any", _handler: "Any") -> "Any":
            after = int(parse_qs(urlsplit(path).query)["after"][0])
            return 200, page([], after + 1, 10), "application/json"

        with fault_server(route) as (origin, server, attempts), \
                patch.object(codex_api_client, "ENTITY_PULL_MAX_PAGES", 2):
            with self.assertRaisesRegex(ValueError, "exceeded its 2-page limit at checkpoint 2"):
                codex_api_client.pull_entities(origin)
            self.assertEqual(len(server.requests), 2)
            self.assertEqual(len(attempts), 2)

    def test_only_unknown_entity_scope_400_is_reported_as_an_old_backend(self: "Any") -> None:
        def route(_method: "Any", _path: "Any", _handler: "Any") -> "Any":
            return 400, {"error": "Invalid sync scope"}, "application/json"

        with fault_server(route) as (origin, _server, _attempts):
            with self.assertRaisesRegex(ValueError, "backend may be older"):
                codex_api_client.pull_entities(origin)

        def bad_query(_method: "Any", _path: "Any", _handler: "Any") -> "Any":
            return 400, {"error": "Invalid query value for after"}, "application/json"

        with fault_server(bad_query) as (origin, _server, _attempts):
            with self.assertRaisesRegex(ValueError, "Invalid query value") as raised:
                codex_api_client.pull_entities(origin)
            self.assertNotIn("backend may be older", str(raised.exception))

        def missing_endpoint(_method: "Any", _path: "Any", _handler: "Any") -> "Any":
            return 404, {"detail": "Not Found"}, "application/json"

        with fault_server(missing_endpoint) as (origin, _server, _attempts):
            with self.assertRaisesRegex(ValueError, "backend may be older"):
                codex_api_client.pull_entities(origin)

    def test_wrong_token_and_unavailable_backend_have_truthful_errors(self: "Any") -> None:
        def unauthorized(_method: "Any", _path: "Any", _handler: "Any") -> "Any":
            return 401, {"error": "Invalid session token"}, "application/json"

        with fault_server(unauthorized) as (origin, _server, _attempts):
            with self.assertRaisesRegex(codex_api_client.StudioHTTPError,
                                        "rejected the session token"):
                codex_api_client.studio_json(origin, "/api/messages", "wrong-token")

        unavailable = socket.socket()
        unavailable.bind(("127.0.0.1", 0))
        port = unavailable.getsockname()[1]
        unavailable.close()
        with self.assertRaises(OSError) as raised:
            codex_api_client.session_token(f"http://127.0.0.1:{port}", timeout=1)
        self.assertRegex(str(raised.exception), "urlopen error|Connection refused")

    def test_non_json_response_is_named_as_non_json(self: "Any") -> None:
        def route(_method: "Any", _path: "Any", _handler: "Any") -> "Any":
            return 200, b"not-json", "text/plain"

        with fault_server(route) as (origin, _server, _attempts):
            with self.assertRaisesRegex(ValueError, "Studio answered with non-JSON"):
                codex_api_client.session_token(origin)

    def test_control_list_merges_wave_files_and_sorts_entity_agents(self: "Any") -> None:
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            (state / "codex-swarm-status.wave.json").write_text(json.dumps([{
                "wave": "wave", "runId": "run", "threadId": "wave-thread",
                "name": "Wave Worker", "turnStatus": "running", "launcherPid": os.getpid(),
                "orchestratorId": "lead-thread", "model": "wave-model",
                "accountKey": "wave-account", "tokensUsed": 23, "createdAt": 3,
            }]), encoding="utf-8")
            agents = [
                {"id": "z-agent", "name": "Z", "status": "waiting", "createdAt": 2},
                {"id": "b-agent", "name": "B", "status": "running", "createdAt": 1},
                {"id": "a-agent", "name": "A", "status": "running", "createdAt": 1,
                 "isLead": True, "threadId": "lead-thread"},
            ]

            def route(_method: "Any", path: "Any", _handler: "Any") -> "Any":
                if path == "/api/session":
                    return 200, {"token": "test-token"}, "application/json"
                if path == "/api/desktop":
                    return 200, {"stateDir": directory}, "application/json"
                query = parse_qs(urlsplit(path).query)
                self.assertEqual(query["scope"], [codex_api_client.ENTITY_SCOPE])
                docs = [entity("agent", agent["id"], agent, index + 1)
                        for index, agent in enumerate(agents)]
                return 200, page(docs, 3, 3), "application/json"

            output, errors = io.StringIO(), io.StringIO()
            with fault_server(route) as (origin, server, attempts), \
                    patch.dict(os.environ, {"CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}), \
                    patch.object(sys, "argv", [str(SCRIPT), "--url", origin, "list"]), \
                    redirect_stdout(output), redirect_stderr(errors):
                try:
                    runpy.run_path(str(SCRIPT), run_name="__main__")
                    status: int | str | None = 0
                except SystemExit as exit_code:
                    status = exit_code.code
            self.assertEqual(status, 0, errors.getvalue())
            rows = json.loads(output.getvalue())
            self.assertEqual([row["id"] for row in rows], [identity(
                "wave", "run", "wave-thread", "Wave Worker"), "a-agent", "b-agent", "z-agent"])
            wave = rows[0]
            self.assertEqual(wave, {
                "id": identity("wave", "run", "wave-thread", "Wave Worker"),
                "name": "Wave Worker", "parent": "a-agent", "status": "running",
                "model": "wave-model", "accountKey": "wave-account", "tokens": 23,
            })
            self.assertEqual(errors.getvalue(), "")
            self.assertEqual([path.split("?", 1)[0] for _, path, _ in server.requests], [
                "/api/session", "/api/sync/pull", "/api/desktop",
            ])
            self.assertEqual(len(attempts), 3)
            self.assertTrue(all(
                token == "test-token" for _, path, token in server.headers
                if path.startswith("/api/sync/pull")
            ))

    def test_control_configure_concurrency_reads_the_entity_revision(self: "Any") -> None:
        lead = {"id": "lead", "name": "Lead", "status": "running", "isLead": True,
                "agentModeRevision": 8}

        def route(method: "Any", path: "Any", handler: "Any") -> "Any":
            if method == "POST":
                return 200, {"ok": True, "requestId": "saved"}, "application/json"
            if path == "/api/session":
                return 200, {"token": "test-token"}, "application/json"
            query = parse_qs(urlsplit(path).query)
            self.assertEqual(query["scope"], [codex_api_client.ENTITY_SCOPE])
            return 200, page([entity("agent", "lead", lead, 1)], 1, 1), "application/json"

        output, errors = io.StringIO(), io.StringIO()
        with fault_server(route) as (origin, server, attempts), \
                patch.dict(os.environ, {"CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}), \
                patch.object(sys, "argv", [str(SCRIPT), "--url", origin, "configure", "lead",
                                            "--concurrency", "3"]), \
                redirect_stdout(output), redirect_stderr(errors):
            try:
                runpy.run_path(str(SCRIPT), run_name="__main__")
                status: int | str | None = 0
            except SystemExit as exit_code:
                status = exit_code.code
        self.assertEqual(status, 0, errors.getvalue())
        self.assertEqual(json.loads(output.getvalue()), {"ok": True, "requestId": "saved"})
        self.assertIn("retry with --request-id ", errors.getvalue())
        post = next(request for request in server.requests if request[0] == "POST")
        self.assertEqual(post[1], "/api/conversation")
        self.assertEqual(post[2]["subagent_concurrency"], 3)
        self.assertEqual(post[2]["expected_mode_revision"], 8)
        self.assertTrue(post[2]["request_id"])
        self.assertIn(post[2]["request_id"], errors.getvalue())
        self.assertEqual(len(attempts), 3)
        self.assertTrue(all(
            token == "test-token" for _, path, token in server.headers
            if path != "/api/session"
        ))

    def test_control_list_prefers_current_wave_file_and_merges_graph_alias(self: "Any") -> None:
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            status_path = state / "codex-swarm-status.wave.json"

            def write_status(status: "Any") -> "Any":
                status_path.write_text(json.dumps([{
                    "wave": "wave", "runId": "run", "threadId": "wave-thread",
                    "name": "Wave Worker", "turnStatus": status,
                    "launcherPid": os.getpid(), "createdAt": 1,
                }]), encoding="utf-8")

            wave_id = identity("wave", "run", "wave-thread", "Wave Worker")
            alias = {"id": "graph-alias", "name": "Alias", "threadId": "wave-thread",
                     "parentId": "lead-thread", "source": "registered", "createdAt": 1}
            child = {"id": "wave-child", "name": "Child", "threadId": "child-thread",
                     "parentId": "graph-alias", "status": "running", "source": "managed",
                     "createdAt": 4}
            stale_wave = {"id": wave_id, "name": "Old wave", "threadId": "wave-thread",
                          "status": "old-status", "source": "app-server", "createdAt": 1}
            stale_reference = {"id": "old-lead-reference", "name": "Old lead",
                               "source": "orchestrator-reference", "status": "unknown"}
            entities = [stale_wave, stale_reference, alias, child]

            def list_agents() -> "Any":
                def route(_method: "Any", path: "Any", _handler: "Any") -> "Any":
                    if path == "/api/session":
                        return 200, {"token": "test-token"}, "application/json"
                    if path == "/api/desktop":
                        return 200, {"stateDir": directory}, "application/json"
                    docs = [entity("agent", agent["id"], agent, seq)
                            for seq, agent in enumerate(entities, 1)]
                    return 200, page(docs, len(docs), len(docs)), "application/json"

                output, errors = io.StringIO(), io.StringIO()
                with fault_server(route) as (origin, _server, _attempts), \
                        patch.dict(os.environ, {"CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}), \
                        patch.object(sys, "argv", [str(SCRIPT), "--url", origin, "list"]), \
                        redirect_stdout(output), redirect_stderr(errors):
                    try:
                        runpy.run_path(str(SCRIPT), run_name="__main__")
                        status: int | str | None = 0
                    except SystemExit as exit_code:
                        status = exit_code.code
                self.assertEqual(status, 0, errors.getvalue())
                return json.loads(output.getvalue()), errors.getvalue()

            write_status("running")
            rows, error = list_agents()
            by_id = {row["id"]: row for row in rows}
            self.assertEqual(len(rows), len(by_id))
            self.assertNotIn("graph-alias", by_id)
            self.assertNotIn("old-lead-reference", by_id)
            self.assertEqual(by_id[wave_id]["name"], "Wave Worker")
            self.assertEqual(by_id[wave_id]["status"], "running")
            self.assertEqual(by_id["wave-child"]["parent"], wave_id)
            self.assertEqual(error, "")

            write_status("completed")
            rows, error = list_agents()
            by_id = {row["id"]: row for row in rows}
            self.assertEqual(by_id[wave_id]["status"], "completed")
            self.assertEqual(len(rows), len(by_id))
            self.assertEqual(error, "")

            status_path.unlink()
            rows, error = list_agents()
            by_id = {row["id"]: row for row in rows}
            self.assertNotIn(wave_id, by_id)
            self.assertNotIn("old-lead-reference", by_id)
            self.assertIn("graph-alias", by_id)
            self.assertEqual(by_id["wave-child"]["parent"], "graph-alias")
            self.assertEqual(len(rows), len(by_id))
            self.assertEqual(error, "")

    def test_bad_status_files_are_one_line_notes_and_preserve_entities(self: "Any") -> None:
        cases = ((b"{invalid", None), (json.dumps([{"name": "missing thread"}]).encode(), None),
                 (json.dumps([]).encode(), 0))
        for data, mode in cases:
            with self.subTest(data=data, mode=mode), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "codex-swarm-status.bad.json"
                path.write_bytes(data)
                if mode is not None:
                    path.chmod(mode)

                def route(_method: "Any", endpoint: "Any", _handler: "Any") -> "Any":
                    if endpoint == "/api/session":
                        return 200, {"token": "token"}, "application/json"
                    if endpoint == "/api/desktop":
                        return 200, {"stateDir": directory}, "application/json"
                    agent = {"id": "runtime", "name": "Runtime", "status": "running"}
                    return 200, page([entity("agent", "runtime", agent, 1)], 1, 1), "application/json"

                output, errors = io.StringIO(), io.StringIO()
                with fault_server(route) as (origin, _server, _attempts), \
                        patch.dict(os.environ, {"CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}), \
                        patch.object(sys, "argv", [str(SCRIPT), "--url", origin, "list"]), \
                        redirect_stdout(output), redirect_stderr(errors):
                    try:
                        runpy.run_path(str(SCRIPT), run_name="__main__")
                        status: int | str | None = 0
                    except SystemExit as exit_code:
                        status = exit_code.code
                self.assertEqual(status, 0, errors.getvalue())
                self.assertEqual([row["id"] for row in json.loads(output.getvalue())], ["runtime"])
                self.assertEqual(errors.getvalue(),
                    "codex-control: file-backed wave threads are not available from this machine\n")

    def test_list_survives_desktop_without_state_directory_or_unavailable_endpoint(self: "Any") -> None:
        for desktop_response in ((200, {"other": "field"}), (404, {"error": "not found"})):
            with self.subTest(desktop_response=desktop_response):
                def route(_method: "Any", path: "Any", _handler: "Any") -> "Any":
                    if path == "/api/session":
                        return 200, {"token": "token"}, "application/json"
                    if path == "/api/desktop":
                        return desktop_response[0], desktop_response[1], "application/json"
                    agent = {"id": "runtime", "name": "Runtime", "status": "running"}
                    return 200, page([entity("agent", "runtime", agent, 1)], 1, 1), "application/json"

                output, errors = io.StringIO(), io.StringIO()
                with fault_server(route) as (origin, _server, _attempts), \
                        patch.dict(os.environ, {"CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}), \
                        patch.object(sys, "argv", [str(SCRIPT), "--url", origin, "list"]), \
                        redirect_stdout(output), redirect_stderr(errors):
                    try:
                        runpy.run_path(str(SCRIPT), run_name="__main__")
                        status: int | str | None = 0
                    except SystemExit as exit_code:
                        status = exit_code.code
                self.assertEqual(status, 0, errors.getvalue())
                self.assertEqual(json.loads(output.getvalue())[0]["id"], "runtime")
                self.assertEqual(errors.getvalue(),
                    "codex-control: file-backed wave threads are not available from this machine\n")

    def test_incomplete_http_reads_are_one_line_errors_for_list_configure_and_send(self: "Any") -> None:
        from http.client import IncompleteRead

        for argv in (("list",), ("configure", "lead", "--concurrency", "2"),
                     ("send", "lead", "hello")):
            with self.subTest(argv=argv):
                output, errors = io.StringIO(), io.StringIO()
                with patch.dict(os.environ, {"CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}), \
                        patch.object(codex_api_client, "open_request",
                                     side_effect=IncompleteRead(b"partial", 20)), \
                        patch.object(sys, "argv", [str(SCRIPT), "--url", "http://127.0.0.1:43123", *argv]), \
                        redirect_stdout(output), redirect_stderr(errors):
                    try:
                        runpy.run_path(str(SCRIPT), run_name="__main__")
                        status: int | str | None = 0
                    except SystemExit as exit_code:
                        status = exit_code.code
                self.assertEqual(status, 1)
                self.assertIn("incomplete response", errors.getvalue())
                self.assertNotIn("Traceback", errors.getvalue())

    def test_http_error_with_json_list_body_has_no_secondary_attribute_error(self: "Any") -> None:
        def route(_method: "Any", path: "Any", _handler: "Any") -> "Any":
            if path == "/api/session":
                return 200, {"token": "token"}, "application/json"
            return 500, ["server error"], "application/json"

        output, errors = io.StringIO(), io.StringIO()
        with fault_server(route) as (origin, _server, _attempts), \
                patch.dict(os.environ, {"CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}), \
                patch.object(sys, "argv", [str(SCRIPT), "--url", origin, "send", "lead", "hello"]), \
                redirect_stdout(output), redirect_stderr(errors):
            try:
                runpy.run_path(str(SCRIPT), run_name="__main__")
                status: int | str | None = 0
            except SystemExit as exit_code:
                status = exit_code.code
        self.assertEqual(status, 1)
        self.assertIn("HTTP Error 500", errors.getvalue())
        self.assertNotIn("AttributeError", errors.getvalue())
        self.assertNotIn("Traceback", errors.getvalue())

    def test_control_list_keeps_entity_agents_when_remote_wave_files_are_unreadable(self: "Any") -> None:
        def route(_method: "Any", path: "Any", _handler: "Any") -> "Any":
            if path == "/api/session":
                return 200, {"token": "test-token"}, "application/json"
            if path == "/api/desktop":
                return 200, {"stateDir": "/remote/host/state/codex-agents"}, "application/json"
            return 200, page([entity("agent", "remote-agent", {
                "id": "remote-agent", "name": "Remote", "status": "running",
            }, 1)], 1, 1), "application/json"

        output, errors = io.StringIO(), io.StringIO()
        with fault_server(route) as (origin, _server, _attempts), \
                patch.dict(os.environ, {"CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}), \
                patch.object(sys, "argv", [str(SCRIPT), "--url", origin, "list"]), \
                redirect_stdout(output), redirect_stderr(errors):
            try:
                runpy.run_path(str(SCRIPT), run_name="__main__")
                status: int | str | None = 0
            except SystemExit as exit_code:
                status = exit_code.code
        self.assertEqual(status, 0, errors.getvalue())
        self.assertEqual(json.loads(output.getvalue())[0]["id"], "remote-agent")
        self.assertEqual(errors.getvalue(),
                         "codex-control: file-backed wave threads are not available from this machine\n")


if __name__ == "__main__":
    unittest.main()
