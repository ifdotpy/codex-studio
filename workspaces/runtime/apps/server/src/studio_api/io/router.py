"""HTTP routes for terminal sessions, command monitors, uploads, and files."""
from __future__ import annotations

import base64
import os
import re
from pathlib import Path
from typing import TYPE_CHECKING, Annotated, BinaryIO, Iterator, Protocol, TypedDict, cast

from fastapi import APIRouter, Header, HTTPException, Query, Request, Response
from fastapi.responses import StreamingResponse
from starlette.types import Receive, Scope, Send

from studio_api.io.models import (
    AssetRecord,
    AssetUpload,
    FileContent,
    FileInfo,
    FileQuery,
    MonitorId,
    MonitorInput,
    MonitorActionResult,
    MonitorLogQuery,
    TerminalClose,
    TerminalCreate,
    TerminalInput,
    TerminalInputResult,
    TerminalList,
    TerminalOutput,
    TerminalRecord,
    TerminalRename,
    TerminalResize,
    TerminalOutputQuery,
)
from studio_api.models import JsonValue
from studio_api.request_helpers import body_data
from studio_api.responses import (
    API_BOUNDARY_RESPONSE_HEADERS,
    API_CONTEXT_RESPONSE_HEADERS,
    api_read_responses,
    api_write_responses,
    error_responses,
    response_headers,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext

LOG_CHUNK_BYTES = 65536
MONITOR_LOG_NAME = re.compile(r"[^A-Za-z0-9._-]")
BYTE_RANGE = re.compile(r"bytes=(\d*)-(\d*)\Z")
_MONITOR_LOG_HEADERS = {
    **API_CONTEXT_RESPONSE_HEADERS,
    **API_BOUNDARY_RESPONSE_HEADERS,
    **response_headers(
        "Content-Length", "Content-Disposition", "Accept-Ranges", "Cache-Control",
        "X-Content-Type-Options", "X-Log-Truncated",
    ),
}
_MONITOR_LOG_ERRORS = error_responses((400, 401, 403, 404, 429, 500, 503))


class MonitorLogData(TypedDict):
    path: Path
    fallback: bytes | None
    size: int
    name: str
    mime: str
    truncated: bool


class TerminalService(Protocol):
    def listing(self) -> JsonValue: ...
    def output(self, key: str | None, offset: str | None) -> JsonValue: ...
    def history_output(self, key: str | None, offset: str | None, limit: str | None) -> JsonValue: ...
    def create(self, runtime: object, data: dict[str, JsonValue]) -> JsonValue: ...
    def action(self, action: str, data: dict[str, JsonValue]) -> JsonValue: ...


class IORuntimeService(Protocol):
    def monitor_log(self, key: str) -> MonitorLogData: ...
    def file_info(self, agent: str | None, path: str | None, asset_id: str | None) -> JsonValue: ...
    def file_content(self, agent: str | None, path: str | None, asset_id: str | None) -> tuple[bytes, str, str]: ...
    def monitor_input(self, key: str, data: dict[str, JsonValue]) -> JsonValue: ...
    def cancel_monitor(self, key: str) -> JsonValue: ...
    def upload_asset(self, data: dict[str, JsonValue]) -> JsonValue: ...


class IOContext(Protocol):
    @property
    def runtime(self) -> IORuntimeService | None: ...
    def terminals(self) -> TerminalService: ...
    def send(self, request: Request, value: object, **kwargs: object) -> Response: ...


class ClosingFileResponse(StreamingResponse):
    """Close an already-open file even if streaming is cancelled before iteration."""

    def __init__(
        self,
        content: Iterator[bytes],
        stream: BinaryIO,
        *,
        status_code: int,
        media_type: str,
        headers: dict[str, str],
    ) -> None:
        super().__init__(content, status_code=status_code, media_type=media_type, headers=headers)
        self._stream = stream

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        try:
            await super().__call__(scope, receive, send)
        finally:
            self._stream.close()


def _runtime(runtime: IORuntimeService | None) -> IORuntimeService:
    if runtime is None:
        raise HTTPException(status_code=404, detail="Not found")
    return runtime


def _monitor_log_response(
    context: IOContext, request: Request, monitor_id: str, range_header: str | None = None
) -> Response:
    download = _runtime(context.runtime).monitor_log(monitor_id)
    path = Path(download["path"])
    fallback = download.get("fallback") or b""
    stream: BinaryIO | None = None
    try:
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        stream = os.fdopen(descriptor, "rb")
        size = os.fstat(stream.fileno()).st_size
    except FileNotFoundError:
        size = len(fallback)

    start, end, status_code = 0, size, 200
    header = range_header
    if header:
        match = BYTE_RANGE.fullmatch(header)
        if not match or (not match.group(1) and not match.group(2)):
            if stream:
                stream.close()
            return Response(status_code=416, headers={"Content-Range": f"bytes */{size}"})
        try:
            if match.group(1):
                start = int(match.group(1))
                end = min(size, int(match.group(2)) + 1) if match.group(2) else size
            else:
                suffix = int(match.group(2))
                start, end = max(0, size - suffix), size
        except ValueError:
            if stream:
                stream.close()
            return Response(status_code=416, headers={"Content-Range": f"bytes */{size}"})
        if start >= size or end <= start:
            if stream:
                stream.close()
            return Response(status_code=416, headers={"Content-Range": f"bytes */{size}"})
        status_code = 206

    length = end - start
    safe_name = MONITOR_LOG_NAME.sub("_", download["name"])
    headers = {
        "Content-Length": str(length),
        "Content-Disposition": f'attachment; filename="{safe_name}"',
        "Accept-Ranges": "bytes",
        "Cache-Control": "no-store",
        "X-Content-Type-Options": "nosniff",
        "X-Log-Truncated": "true" if download.get("truncated") else "false",
    }
    if status_code == 206:
        headers["Content-Range"] = f"bytes {start}-{end - 1}/{size}"

    if stream:
        opened = stream
        def iterate_file() -> Iterator[bytes]:
            try:
                opened.seek(start)
                remaining = length
                while remaining:
                    chunk = opened.read(min(LOG_CHUNK_BYTES, remaining))
                    if not chunk:
                        break
                    remaining -= len(chunk)
                    yield chunk
            finally:
                opened.close()

        body: Iterator[bytes] = iterate_file()
        return ClosingFileResponse(body, opened, status_code=status_code, media_type=download.get("mime", "text/plain"), headers=headers)
    else:
        body = iter((fallback[start:end],))
    return StreamingResponse(body, status_code=status_code, media_type=download.get("mime", "text/plain"), headers=headers)


def create_router(context: ApiContext) -> APIRouter:
    io_context = cast(IOContext, context)
    router = APIRouter()

    @router.get("/api/terminals", response_model=TerminalList, responses=api_read_responses(errors=(400,)))
    def list_terminals(request: Request) -> Response:
        return io_context.send(request, io_context.terminals().listing())

    @router.get("/api/terminals/output", response_model=TerminalOutput, responses=api_read_responses(errors=(400,)))
    def terminal_output(request: Request, _query: Annotated[TerminalOutputQuery, Query()]) -> Response:
        manager = io_context.terminals()
        if _query.history == "1":
            result = manager.history_output(
                _query.id, str(_query.offset), str(_query.limit)
            )
        else:
            result = manager.output(_query.id, str(_query.offset))
        return io_context.send(request, result)

    @router.get(
        "/api/monitor/log",
        response_class=Response,
        responses={
            200: {
                "content": {"text/plain": {"schema": {"type": "string"}}},
                "headers": _MONITOR_LOG_HEADERS,
            },
            206: {
                "content": {"text/plain": {"schema": {"type": "string"}}},
                "headers": {**_MONITOR_LOG_HEADERS, **response_headers("Content-Range")},
            },
            416: {
                "description": "Requested byte range is not satisfiable",
                "headers": {
                    **API_CONTEXT_RESPONSE_HEADERS,
                    **API_BOUNDARY_RESPONSE_HEADERS,
                    **response_headers("Content-Length", "Content-Range"),
                },
            },
            **_MONITOR_LOG_ERRORS,
        },
    )
    def monitor_log(
        request: Request,
        query: Annotated[MonitorLogQuery, Query()],
        range_header: str | None = Header(
            default=None,
            alias="Range",
        ),
    ) -> Response:
        monitor_id = query.id
        return _monitor_log_response(io_context, request, monitor_id, range_header)

    @router.get("/api/file-info", response_model=FileInfo, responses=api_read_responses(errors=(400, 404)))
    def file_info(request: Request, query: Annotated[FileQuery, Query()]) -> Response:
        return io_context.send(request, _runtime(io_context.runtime).file_info(query.agent, query.path, query.asset))

    @router.get("/api/file", response_model=FileContent, responses=api_read_responses(errors=(400, 404)))
    def file_content(request: Request, query: Annotated[FileQuery, Query()]) -> Response:
        content, mime, name = _runtime(io_context.runtime).file_content(query.agent, query.path, query.asset)
        return io_context.send(request, {"name": name, "mime": mime, "base64": base64.b64encode(content).decode()})

    @router.post("/api/terminals/create", response_model=TerminalRecord, responses=api_write_responses(errors=(400,)))
    def create_terminal(request: Request, body: TerminalCreate) -> Response:
        manager = io_context.terminals()
        runtime = io_context.runtime
        if runtime is None:
            raise ValueError("The agent runtime is unavailable")
        result = manager.create(runtime, body_data(body))
        return io_context.send(request, result)

    @router.post("/api/terminals/input", response_model=TerminalInputResult, responses=api_write_responses(errors=(400,)))
    def terminal_input(request: Request, body: TerminalInput) -> Response:
        return io_context.send(request, io_context.terminals().action("input", body_data(body)))

    @router.post("/api/terminals/resize", response_model=TerminalRecord, responses=api_write_responses(errors=(400,)))
    def resize_terminal(request: Request, body: TerminalResize) -> Response:
        return io_context.send(request, io_context.terminals().action("resize", body_data(body)))

    @router.post("/api/terminals/rename", response_model=TerminalRecord, responses=api_write_responses(errors=(400,)))
    def rename_terminal(request: Request, body: TerminalRename) -> Response:
        return io_context.send(request, io_context.terminals().action("rename", body_data(body)))

    @router.post("/api/terminals/close", response_model=TerminalRecord, responses=api_write_responses(errors=(400,)))
    def close_terminal(request: Request, body: TerminalClose) -> Response:
        return io_context.send(request, io_context.terminals().action("close", body_data(body)))

    @router.post("/api/monitor/input", response_model=JsonValue, responses=api_write_responses(errors=(400,)))
    def monitor_input(request: Request, body: MonitorInput) -> Response:
        return io_context.send(request, _runtime(io_context.runtime).monitor_input(body.id, body_data(body)))

    @router.post("/api/monitor/cancel", response_model=MonitorActionResult, responses=api_write_responses(errors=(400,)))
    def cancel_monitor(request: Request, body: MonitorId) -> Response:
        return io_context.send(request, _runtime(io_context.runtime).cancel_monitor(body.id))

    @router.post("/api/assets", response_model=AssetRecord, responses=api_write_responses(errors=(400,)))
    def upload_asset(request: Request, body: AssetUpload) -> Response:
        return io_context.send(request, _runtime(io_context.runtime).upload_asset(body_data(body)))

    return router
