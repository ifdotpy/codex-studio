"""HTTP routes for terminal sessions, command monitors, uploads, and files."""
from __future__ import annotations

import base64
import os
import re
from pathlib import Path
from typing import TYPE_CHECKING, Annotated, BinaryIO, Iterator, Protocol, TypedDict, cast

from fastapi import APIRouter, HTTPException, Query, Request, Response
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
from studio_api.models import ContractModel, ErrorResponse, JsonValue

if TYPE_CHECKING:
    from studio_api.context import ApiContext

LOG_CHUNK_BYTES = 65536
MONITOR_LOG_NAME = re.compile(r"[^A-Za-z0-9._-]")
BYTE_RANGE = re.compile(r"bytes=(\d*)-(\d*)\Z")


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


def _first_query(request: Request, name: str, default: str | None = None) -> str | None:
    # urllib.parse.parse_qs (used by the legacy handler) omits blank values.
    values = [value for value in request.query_params.getlist(name) if value]
    return values[0] if values else default


def _runtime(runtime: IORuntimeService | None) -> IORuntimeService:
    if runtime is None:
        raise HTTPException(status_code=404, detail="Not found")
    return runtime


def _request_data(model: ContractModel) -> dict[str, JsonValue]:
    """Pydantic JSON-mode dump, preserving only supplied request fields."""
    return cast(dict[str, JsonValue], model.model_dump(mode="json", exclude_unset=True))


def _monitor_log_response(context: IOContext, request: Request, monitor_id: str) -> Response:
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
    header = request.headers.get("range")
    if header:
        match = BYTE_RANGE.fullmatch(header.strip())
        if not match or (not match.group(1) and not match.group(2)):
            if stream:
                stream.close()
            return Response(status_code=416, headers={"Content-Range": f"bytes */{size}"})
        if match.group(1):
            start = int(match.group(1))
            end = min(size, int(match.group(2)) + 1) if match.group(2) else size
        else:
            suffix = int(match.group(2))
            start, end = max(0, size - suffix), size
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

    @router.get("/api/terminals", response_model=TerminalList, responses={400: {"model": ErrorResponse}})
    def list_terminals(request: Request) -> Response:
        return io_context.send(request, io_context.terminals().listing())

    @router.get("/api/terminals/output", response_model=TerminalOutput, responses={400: {"model": ErrorResponse}})
    def terminal_output(request: Request, _query: Annotated[TerminalOutputQuery, Query()]) -> Response:
        manager = io_context.terminals()
        key = _first_query(request, "id")
        offset = _first_query(request, "offset", "0")
        if [value for value in request.query_params.getlist("history") if value] == ["1"]:
            result = manager.history_output(key, offset, _first_query(request, "limit", "65536"))
        else:
            result = manager.output(key, offset)
        return io_context.send(request, result)

    @router.get("/api/monitor/log", responses={200: {"content": {"text/plain": {}}}, 206: {"content": {"text/plain": {}}}, 400: {"model": ErrorResponse}})
    def monitor_log(request: Request, query: Annotated[MonitorLogQuery, Query()]) -> Response:
        monitor_id = _first_query(request, "id")
        if monitor_id is None:
            raise ValueError("Unknown monitor")
        return _monitor_log_response(io_context, request, monitor_id)

    @router.get("/api/file-info", response_model=FileInfo, responses={400: {"model": ErrorResponse}, 404: {"model": ErrorResponse}})
    def file_info(request: Request, _query: Annotated[FileQuery, Query()]) -> Response:
        query = FileQuery.model_validate({
            "agent": _first_query(request, "agent"),
            "path": _first_query(request, "path"),
            "asset": _first_query(request, "asset"),
        })
        return io_context.send(request, _runtime(io_context.runtime).file_info(query.agent, query.path, query.asset))

    @router.get("/api/file", response_model=FileContent, responses={400: {"model": ErrorResponse}, 404: {"model": ErrorResponse}})
    def file_content(request: Request, _query: Annotated[FileQuery, Query()]) -> Response:
        query = FileQuery.model_validate({
            "agent": _first_query(request, "agent"),
            "path": _first_query(request, "path"),
            "asset": _first_query(request, "asset"),
        })
        content, mime, name = _runtime(io_context.runtime).file_content(query.agent, query.path, query.asset)
        return io_context.send(request, {"name": name, "mime": mime, "base64": base64.b64encode(content).decode()})

    @router.post("/api/terminals/create", response_model=TerminalRecord, responses={400: {"model": ErrorResponse}})
    def create_terminal(request: Request, body: TerminalCreate) -> Response:
        manager = io_context.terminals()
        runtime = io_context.runtime
        if runtime is None:
            raise ValueError("The agent runtime is unavailable")
        result = manager.create(runtime, _request_data(body))
        return io_context.send(request, result)

    @router.post("/api/terminals/input", response_model=TerminalInputResult, responses={400: {"model": ErrorResponse}})
    def terminal_input(request: Request, body: TerminalInput) -> Response:
        return io_context.send(request, io_context.terminals().action("input", _request_data(body)))

    @router.post("/api/terminals/resize", response_model=TerminalRecord, responses={400: {"model": ErrorResponse}})
    def resize_terminal(request: Request, body: TerminalResize) -> Response:
        return io_context.send(request, io_context.terminals().action("resize", _request_data(body)))

    @router.post("/api/terminals/rename", response_model=TerminalRecord, responses={400: {"model": ErrorResponse}})
    def rename_terminal(request: Request, body: TerminalRename) -> Response:
        return io_context.send(request, io_context.terminals().action("rename", _request_data(body)))

    @router.post("/api/terminals/close", response_model=TerminalRecord, responses={400: {"model": ErrorResponse}})
    def close_terminal(request: Request, body: TerminalClose) -> Response:
        return io_context.send(request, io_context.terminals().action("close", _request_data(body)))

    @router.post("/api/monitor/input", response_model=JsonValue, responses={400: {"model": ErrorResponse}})
    def monitor_input(request: Request, body: MonitorInput) -> Response:
        return io_context.send(request, _runtime(io_context.runtime).monitor_input(body.id, _request_data(body)))

    @router.post("/api/monitor/cancel", response_model=MonitorActionResult, responses={400: {"model": ErrorResponse}})
    def cancel_monitor(request: Request, body: MonitorId) -> Response:
        return io_context.send(request, _runtime(io_context.runtime).cancel_monitor(body.id))

    @router.post("/api/assets", response_model=AssetRecord, responses={400: {"model": ErrorResponse}})
    def upload_asset(request: Request, body: AssetUpload) -> Response:
        return io_context.send(request, _runtime(io_context.runtime).upload_asset(_request_data(body)))

    return router
