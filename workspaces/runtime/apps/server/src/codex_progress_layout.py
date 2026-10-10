"""Store current renderer measurements beside the agent's progress file."""
import argparse
from codex_file_lock import flock, LOCK_EX, LOCK_NB
import hashlib
import json
import math
import os
from pathlib import Path
import shlex
import stat
import sys
import time
import uuid

from codex_progress import _directory, progress_path, read_progress
from codex_private_paths import protect_temp_file, verify_handle_within_directory

RENDERER = "progress-markdown-v2"
LEGACY_RENDERER = "progress-markdown-v1"
VISIBILITY_FIELDS = {"overflowX", "overflowY", "totalLines", "visibleLines",
                     "lastVisibleLine", "lastVisibleHeading"}
FRESH_SECONDS = 90
MAX_CLIENTS = 32
LAYOUT_FILE = "PROGRESS.layout.json"
MAX_LAYOUT_BYTES = 65536


class LayoutConflict(ValueError):
    pass


def layout_path(state_dir, agent_id):
    return progress_path(state_dir, agent_id).with_name(LAYOUT_FILE)


def _open_layout_file(directory: int | Path, name: str, flags: int, mode: int = 0o600) -> int:
    if not isinstance(directory, Path):
        return os.open(name, flags, mode, dir_fd=directory)
    target = directory / name
    try:
        before = target.lstat()
    except FileNotFoundError:
        before = None
    if before is not None and (not stat.S_ISREG(before.st_mode)
                               or getattr(before, "st_file_attributes", 0) & 0x400):
        raise ValueError("Progress layout file must be a regular file")
    descriptor = os.open(target, flags | getattr(os, "O_BINARY", 0), mode)
    try:
        verify_handle_within_directory(descriptor, directory)
        opened = os.fstat(descriptor)
        after = target.lstat()
        if (not stat.S_ISREG(opened.st_mode)
                or getattr(after, "st_file_attributes", 0) & 0x400
                or (opened.st_dev, opened.st_ino) != (after.st_dev, after.st_ino)):
            raise ValueError("Progress layout file must be a stable regular file")
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


def _read_layout(directory: int | Path):
    try:
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        descriptor = _open_layout_file(directory, LAYOUT_FILE, flags)
    except FileNotFoundError:
        return {}
    with os.fdopen(descriptor, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("Progress layout feedback must be a regular file")
        data = stream.read(MAX_LAYOUT_BYTES + 1)
    if len(data) > MAX_LAYOUT_BYTES:
        raise ValueError("Progress layout feedback exceeds its size limit")
    try:
        value = json.loads(data)
    except (UnicodeError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def _fresh_reports(value, now):
    reports = value.get("reports", [])
    if not isinstance(reports, list):
        return []
    fresh = []
    for row in reports:
        if (not isinstance(row, dict) or type(row.get("measuredAt")) not in (int, float)
                or not math.isfinite(row["measuredAt"])
                or not 0 <= now - row["measuredAt"] < FRESH_SECONDS):
            continue
        try:
            _report({"agent": "feedback", "revision": "feedback", **{
                key: value for key, value in row.items() if key not in {"measuredAt", "expiresAt"}}})
        except (ValueError, TypeError):
            continue
        fresh.append(row)
    return fresh


def _status(reports):
    if not reports:
        return "unmeasured"
    if any(row["reason"] == "unsupported" or
           (row["renderer"] == LEGACY_RENDERER and not row["fits"]) for row in reports):
        return "does_not_fit"
    return "fits" if all(row["fits"] for row in reports) else "clipped"


def _report(body):
    fields = {"agent", "revision", "client", "sequence", "renderer", "width", "height",
              "contentWidth", "contentHeight", "fits", "reason"}
    if isinstance(body, dict) and body.get("renderer") == RENDERER:
        fields |= VISIBILITY_FIELDS
    if not isinstance(body, dict) or set(body) != fields:
        raise ValueError("Supply the exact progress layout fields")
    if body["renderer"] not in (RENDERER, LEGACY_RENDERER):
        raise ValueError("Unsupported progress renderer")
    if not isinstance(body["revision"], str) or not 1 <= len(body["revision"]) <= 200:
        raise ValueError("A progress file revision is required")
    if not isinstance(body["client"], str) or not 1 <= len(body["client"]) <= 100 or not all(
            c.isascii() and (c.isalnum() or c in "-_") for c in body["client"]):
        raise ValueError("Invalid progress client identity")
    if type(body["sequence"]) is not int or not 0 <= body["sequence"] <= 9007199254740991:
        raise ValueError("Invalid progress measurement sequence")
    for field in ("width", "height", "contentWidth", "contentHeight"):
        number = body[field]
        if type(number) not in (float, int) or not math.isfinite(number) or not 0 <= number <= 10000000:
            raise ValueError("Invalid progress measurement dimensions")
    if not 0 < body["width"] <= 100000 or not 0 < body["height"] <= (280 if body["renderer"] == RENDERER else 150):
        raise ValueError("Invalid progress panel size")
    if type(body["fits"]) is not bool or body["reason"] not in (None, "overflow", "unsupported"):
        raise ValueError("Invalid progress fit result")
    if body["fits"] and (body["reason"] is not None
            or body["contentWidth"] > body["width"] + 0.5
            or body["contentHeight"] > body["height"] + 0.5):
        raise ValueError("Progress dimensions contradict a successful fit")
    if not body["fits"] and body["reason"] is None:
        raise ValueError("A failed fit needs a reason")
    if body["renderer"] == RENDERER:
        for field, dimension, content in (("overflowX", "width", "contentWidth"),
                                         ("overflowY", "height", "contentHeight")):
            number = body[field]
            if (type(number) not in (float, int) or not math.isfinite(number)
                    or number < 0 or abs(number - max(0, body[content] - body[dimension])) > 0.5):
                raise ValueError("Invalid progress overflow dimensions")
        if any(type(body[field]) is not int or not 0 <= body[field] <= 2048
               for field in ("totalLines", "visibleLines")) or body["visibleLines"] > body["totalLines"]:
            raise ValueError("Invalid progress visible line counts")
        for field in ("lastVisibleLine", "lastVisibleHeading"):
            if body[field] is not None and (not isinstance(body[field], str) or len(body[field]) > 200):
                raise ValueError("Invalid progress visible line description")
        if body["fits"] and body["visibleLines"] != body["totalLines"]:
            raise ValueError("A successful fit must show all progress lines")
    return {key: body[key] for key in fields - {"agent", "revision"}}


def record_layout(runtime, body):
    report = _report(body)
    agent_id = body["agent"]
    # File access does not hold the shared runtime or SQLite lock.
    with runtime.lock, runtime.db() as db:
        runtime.checked_actor(db, agent_id)
    with _directory(runtime.root, agent_id) as directory:
        flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
        lock = _open_layout_file(directory, ".layout.lock", flags)
        if isinstance(directory, Path):
            protect_temp_file(directory / ".layout.lock")
        temporary = None
        try:
            if not stat.S_ISREG(os.fstat(lock).st_mode):
                raise ValueError("Progress layout lock must be a regular file")
            try:
                flock(lock, LOCK_EX | LOCK_NB)
            except BlockingIOError as error:
                raise LayoutConflict("Another layout measurement is being saved. Retry") from error
            current = read_progress(runtime.root, agent_id)
            if current["error"] or not current["exists"] or current["revision"] != body["revision"]:
                raise LayoutConflict("The progress file changed. Measure its current revision")
            now = time.time()
            previous = _read_layout(directory)
            digest = hashlib.sha256(current["markdown"].encode("utf-8")).hexdigest()
            matching = (previous.get("version") == 1 and previous.get("agent") == agent_id
                        and previous.get("revision") == current["revision"] and previous.get("sha256") == digest)
            reports = _fresh_reports(previous, now) if matching else []
            for row in reports:
                if row["client"] == report["client"] and row["sequence"] >= report["sequence"]:
                    if row["sequence"] == report["sequence"] and all(row.get(key) == value for key, value in report.items()):
                        # A lost response can repeat the same measurement without refreshing its age.
                        return {**previous, "status": _status(reports), "reports": reports}
                    raise LayoutConflict("A newer layout measurement already exists")
            others = [row for row in reports if row.get("client") != report["client"]]
            if len(others) >= MAX_CLIENTS:
                raise LayoutConflict("Too many active progress clients. Retry after old reports expire")
            report["measuredAt"] = now
            report["expiresAt"] = now + FRESH_SECONDS
            reports = others + [report]
            result = {"version": 1, "agent": agent_id, "revision": current["revision"],
                      "sha256": digest,
                      "status": _status(reports), "updatedAt": now, "reports": reports}
            data = (json.dumps(result, ensure_ascii=True, indent=2) + "\n").encode()
            if len(data) > MAX_LAYOUT_BYTES:
                raise ValueError("Progress layout feedback exceeds its size limit")
            temporary = ".layout-" + uuid.uuid4().hex + ".tmp"
            descriptor = _open_layout_file(directory, temporary,
                                           os.O_WRONLY | os.O_CREAT | os.O_EXCL
                                           | getattr(os, "O_NOFOLLOW", 0), 0o444)
            with os.fdopen(descriptor, "wb") as stream:
                # The mode argument is filtered by umask. Restore the exact
                # read-only mode required by the renderer feedback contract.
                if isinstance(directory, Path):
                    protect_temp_file(directory / temporary)
                else:
                    os.fchmod(stream.fileno(), 0o444)
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            if isinstance(directory, Path):
                os.chmod(directory / temporary, 0o444)
            # Never label a later direct file edit with the earlier measurement.
            if read_progress(runtime.root, agent_id)["revision"] != current["revision"]:
                raise LayoutConflict("The progress file changed while saving its measurement")
            if isinstance(directory, Path):
                target = directory / LAYOUT_FILE
                if target.exists():
                    os.chmod(target, 0o600)
                os.replace(directory / temporary, target)
            else:
                os.replace(temporary, LAYOUT_FILE, src_dir_fd=directory, dst_dir_fd=directory)
            temporary = None
            if not isinstance(directory, Path):
                os.fsync(directory)
            return result
        finally:
            if temporary is not None:
                if isinstance(directory, Path):
                    (directory / temporary).unlink(missing_ok=True)
                else:
                    os.unlink(temporary, dir_fd=directory)
            os.close(lock)


def layout_status(state_dir, agent_id):
    current = read_progress(state_dir, agent_id)
    if current["error"]:
        return {"status": "does_not_fit", "error": current["error"], "reports": []}
    if not current["markdown"].strip():
        return {"status": "empty", "reports": []}
    with _directory(state_dir, agent_id) as directory:
        value = _read_layout(directory)
    latest = read_progress(state_dir, agent_id)
    if latest["error"] or latest["revision"] != current["revision"]:
        return {"status": "unmeasured", "reports": [],
                "note": "The progress file changed during the check. Check its current revision again."}
    digest = hashlib.sha256(current["markdown"].encode("utf-8")).hexdigest()
    matching = (value.get("version") == 1 and value.get("agent") == agent_id
                and value.get("revision") == current["revision"] and value.get("sha256") == digest)
    reports = _fresh_reports(value, time.time()) if matching else []
    result = {"status": _status(reports), "revision": current["revision"], "sha256": digest,
              "reports": reports,
              "note": "Only current, recent visible-client measurements count. Without a renderer, fit is unmeasured."}
    measured = [row for row in reports if row["renderer"] == RENDERER and row["reason"] != "unsupported"]
    if measured:
        # Describe the least visible client; keep every report and its identity.
        limiting = min(measured, key=lambda row: (row["visibleLines"], -row["overflowY"], -row["overflowX"]))
        hidden = limiting["totalLines"] - limiting["visibleLines"]
        hint = f"Visible: first {limiting['visibleLines']} top-level lines/items."
        if hidden:
            label = limiting["lastVisibleHeading"] or limiting["lastVisibleLine"]
            hint += f" Hidden: {hidden} lines/items" + (f" below {label}." if label else ".")
        if limiting["overflowX"] or limiting["overflowY"]:
            hint += f" Overflow: {limiting['overflowY']:.0f}px down, {limiting['overflowX']:.0f}px across."
        result = {"status": result["status"], "hint": hint, **result}
    if result["status"] == "unmeasured":
        result["reason"], result["next"] = (
            ("expired", "No visible client measured this revision recently. "
                        "Keep a short status. Fit stays unmeasured.") if matching else
            ("older_revision", "A client measured an earlier revision and has not read this edit yet. "
                               "Check once more after a few seconds. Do not loop.") if value.get("version") == 1 else
            ("no_client", "No visible Studio client has measured this panel. Keep a short status. Fit stays unmeasured."))
    return result


def progress_fit_context(state_dir, agent_id):
    path = progress_path(state_dir, agent_id)
    command = shlex.join([sys.executable, str(Path(__file__).resolve()), str(path), "--wait", "3"])
    return (
        " Put the current status first, then verified results, a next step or a blocker. "
        "Studio shows the top of long content clipped with a fade; users can expand it and scroll. "
        "The collapsed budget scales with viewport height, up to 280px. Overflow is allowed. "
        "Use passive Markdown text, headings, lists, inline code and links. Do not use images, tables, fenced code or raw HTML. "
        f"Studio writes renderer feedback to {path.with_name(LAYOUT_FILE)}. Read it, do not edit it. "
        f"After editing PROGRESS.md, run {command} once to check which top-level lines or list items are visible. "
        "Trim only when important lines are hidden. Do not repeat rewrites just to remove overflow. "
        "Exit 0 means the current revision is visible, including clipped content (or is empty); "
        "1 means unsupported content or a file-read error; 2 means no current measurement exists. "
        "The hint describes the least visible current client. Exact revisions and recent clients remain separate. "
        "Do not loop or wake another agent. Put details in another file or the chat. "
    )


def main():
    parser = argparse.ArgumentParser(description="Read current Studio progress layout feedback without starting a browser")
    parser.add_argument("file", type=Path)
    parser.add_argument("--wait", type=float, default=0,
                        help="Wait at most this many seconds for a visible renderer (maximum 5)")
    args = parser.parse_args()
    if not math.isfinite(args.wait) or not 0 <= args.wait <= 5:
        parser.error("--wait must be between 0 and 5 seconds")
    path = args.file.absolute()
    try:
        if path.name != "PROGRESS.md" or path.parent.parent.name != "progress":
            raise ValueError("Use the exact per-agent PROGRESS.md path from Studio")
        started = time.monotonic()
        deadline = started + args.wait
        while True:
            result = layout_status(path.parent.parent.parent, path.parent.name)
            if result["status"] != "unmeasured" or time.monotonic() >= deadline:
                break
            time.sleep(min(0.1, max(0, deadline - time.monotonic())))
        if args.wait and result['status'] == 'unmeasured':
            result['waitedSeconds'] = round(time.monotonic() - started, 3)
            result['next'] = ('No client measured the current revision during this wait. '
                              'Keep a short status until a visible client measures it.')
    except (OSError, ValueError) as error:
        result = {"status": "unmeasured", "error": str(error)}
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return {"fits": 0, "clipped": 0, "empty": 0, "does_not_fit": 1}.get(result["status"], 2)


if __name__ == "__main__":
    raise SystemExit(main())
