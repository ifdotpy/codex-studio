"""Store current renderer measurements beside the agent's progress file."""
import argparse
import fcntl
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

RENDERER = "progress-markdown-v1"
FRESH_SECONDS = 90
MAX_CLIENTS = 32
LAYOUT_FILE = "PROGRESS.layout.json"
MAX_LAYOUT_BYTES = 65536


class LayoutConflict(ValueError):
    pass


def layout_path(state_dir, agent_id):
    return progress_path(state_dir, agent_id).with_name(LAYOUT_FILE)


def _read_layout(directory):
    try:
        descriptor = os.open(LAYOUT_FILE, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
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
    return "fits" if all(row["fits"] for row in reports) else "does_not_fit"


def _report(body):
    fields = {"agent", "revision", "client", "sequence", "renderer", "width", "height",
              "contentWidth", "contentHeight", "fits", "reason"}
    if not isinstance(body, dict) or set(body) != fields:
        raise ValueError("Supply the exact progress layout fields")
    if body["renderer"] != RENDERER:
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
    if not 0 < body["width"] <= 100000 or not 0 < body["height"] <= 150:
        raise ValueError("Invalid progress panel size")
    if type(body["fits"]) is not bool or body["reason"] not in (None, "overflow", "unsupported"):
        raise ValueError("Invalid progress fit result")
    if body["fits"] and (body["reason"] is not None
            or body["contentWidth"] > body["width"] + 0.5
            or body["contentHeight"] > body["height"] + 0.5):
        raise ValueError("Progress dimensions contradict a successful fit")
    if not body["fits"] and body["reason"] is None:
        raise ValueError("A failed fit needs a reason")
    return {key: body[key] for key in fields - {"agent", "revision"}}


def record_layout(runtime, body):
    report = _report(body)
    agent_id = body["agent"]
    # File access does not hold the shared runtime or SQLite lock.
    with runtime.lock, runtime.db() as db:
        runtime.checked_actor(db, agent_id)
    with _directory(runtime.root, agent_id) as directory:
        lock = os.open(".layout.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK,
                       0o600, dir_fd=directory)
        temporary = None
        try:
            if not stat.S_ISREG(os.fstat(lock).st_mode):
                raise ValueError("Progress layout lock must be a regular file")
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
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
            descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                 0o444, dir_fd=directory)
            with os.fdopen(descriptor, "wb") as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            # Never label a later direct file edit with the earlier measurement.
            if read_progress(runtime.root, agent_id)["revision"] != current["revision"]:
                raise LayoutConflict("The progress file changed while saving its measurement")
            os.replace(temporary, LAYOUT_FILE, src_dir_fd=directory, dst_dir_fd=directory)
            temporary = None
            os.fsync(directory)
            return result
        finally:
            if temporary is not None:
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
    return {"status": _status(reports), "revision": current["revision"], "sha256": digest,
            "reports": reports,
            "note": "Only current, recent visible-client measurements count. Without a renderer, fit is unmeasured."}


def progress_fit_context(state_dir, agent_id):
    path = progress_path(state_dir, agent_id)
    command = shlex.join([sys.executable, str(Path(__file__).resolve()), str(path), "--wait", "3"])
    return (
        " The progress panel has no scroll. Its available space depends on the window and font. "
        "Use a short current status, verified result, next step or blocker. Put details in another file or the chat. "
        "Use passive Markdown text, headings, lists, inline code and links. Do not use images, tables, fenced code or raw HTML. "
        f"Studio writes renderer feedback to {path.with_name(LAYOUT_FILE)}. Read it, do not edit it. "
        f"After editing PROGRESS.md, run {command}. Shorten or simplify until the current revision fits. "
        "Check the required and available pixel sizes, not a guessed number of lines or characters. "
        "Exit 0 means the current revision fits recent visible clients (or is empty); 1 means it does not fit; "
        "2 means no current measurement exists. Allow a visible client to read the new file before another check. "
        "If no client is open, leave a short status and treat fit as unmeasured. Do not loop or wake another agent. "
        "A later narrower window can require a shorter status. Studio shows a fit notice instead of partial content."
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
        deadline = time.monotonic() + args.wait
        while True:
            result = layout_status(path.parent.parent.parent, path.parent.name)
            if result["status"] != "unmeasured" or time.monotonic() >= deadline:
                break
            time.sleep(min(0.1, max(0, deadline - time.monotonic())))
    except (OSError, ValueError) as error:
        result = {"status": "unmeasured", "error": str(error)}
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return {"fits": 0, "empty": 0, "does_not_fit": 1}.get(result["status"], 2)


if __name__ == "__main__":
    raise SystemExit(main())
