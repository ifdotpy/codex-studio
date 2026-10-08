"""Read positive input receipts from the owning native account's saved history."""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import time

READ_LIMIT = 2 * 1024 * 1024
SCAN_LIMIT = 32 * 1024 * 1024


def _open_rollout(home, relative):
    """Open a regular file without following any account-relative symlink."""
    if os.name == 'nt':
        # Windows does not provide dir_fd, O_DIRECTORY, or O_NOFOLLOW. The
        # account directory is private, so validate every path component and
        # the opened file identity before using the handle.
        current = Path(home)
        components = [current]
        for part in relative.parts:
            current = current / part
            components.append(current)
        for directory in components[:-1]:
            info = directory.lstat()
            if (not stat.S_ISDIR(info.st_mode)
                    or getattr(info, 'st_file_attributes', 0) & 0x400):
                raise ValueError('The native rollout path contains a reparse point')
        target = components[-1]
        info = target.lstat()
        if getattr(info, 'st_file_attributes', 0) & 0x400:
            raise ValueError('The native rollout is a reparse point')
        descriptor = os.open(target, os.O_RDONLY | getattr(os, 'O_BINARY', 0))
        try:
            opened = os.fstat(descriptor)
            current_info = target.lstat()
            if (not stat.S_ISREG(opened.st_mode)
                    or getattr(current_info, 'st_file_attributes', 0) & 0x400
                    or (opened.st_dev, opened.st_ino) != (current_info.st_dev, current_info.st_ino)):
                raise ValueError('The native rollout is not a stable regular file')
            return os.fdopen(descriptor, 'rb')
        except BaseException:
            os.close(descriptor)
            raise
    flags = os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC
    directory = os.open(home, flags | os.O_DIRECTORY)
    try:
        for part in relative.parts[:-1]:
            child = os.open(part, flags | os.O_DIRECTORY, dir_fd=directory)
            os.close(directory)
            directory = child
        descriptor = os.open(relative.parts[-1], flags, dir_fd=directory)
        try:
            if not stat.S_ISREG(os.fstat(descriptor).st_mode):
                raise ValueError('The native rollout is not a regular file')
            return os.fdopen(descriptor, 'rb')
        except BaseException:
            os.close(descriptor)
            raise
    finally:
        os.close(directory)


def _database(home, name, deadline):
    path = (home / name).resolve(strict=True)
    path.relative_to(home)
    db = sqlite3.connect(path.as_uri() + '?mode=ro', uri=True, timeout=0.1)
    db.row_factory = sqlite3.Row
    db.set_progress_handler(lambda: time.monotonic() >= deadline, 1000)
    return db


def _saved_item(stream, thread_id, row, item, deadline, budget):
    """The index is a locator. The current rollout must contain the acceptance."""
    offset = row['rollout_byte_offset']
    ordinal = row['turn_ordinal']
    target = row['item_ordinal']
    if (type(offset) is not int or offset < 0 or type(ordinal) is not int
            or type(target) is not int or target <= ordinal):
        return None
    stream.seek(offset)
    saved, digest, previous = 0, hashlib.sha256(), ordinal - 1
    while budget['remaining'] > 0 and time.monotonic() < deadline:
        raw = stream.readline(min(READ_LIMIT, budget['remaining']) + 1)
        budget['remaining'] -= len(raw)
        if not raw.endswith(b'\n') or len(raw) > READ_LIMIT or budget['remaining'] < 0:
            return None
        saved += len(raw)
        digest.update(raw)
        record = json.loads(raw)
        current = record.get('ordinal')
        if type(current) is not int or current != previous + 1 or current > target:
            return None
        previous = current
        payload = record.get('payload') or {}
        if current == ordinal:
            started = (record.get('type') == 'event_msg'
                       and payload.get('type') == 'task_started'
                       and payload.get('turn_id') == row['turn_id'])
            if not started:
                return None
        elif payload.get('type') in {'task_started', 'task_complete', 'turn_aborted'}:
            return None
        if current != target:
            continue
        native = payload.get('item') or {}
        expected = {'type': 'UserMessage', 'id': item['id'],
                    'client_id': item['clientId'], 'content': item['content']}
        if (record.get('type') != 'event_msg' or payload.get('type') != 'item_completed'
                or payload.get('thread_id') != thread_id
                or payload.get('turn_id') != row['turn_id'] or native != expected):
            return None
        stream.seek(offset)
        remaining, check = saved, hashlib.sha256()
        while remaining and time.monotonic() < deadline:
            raw = stream.read(min(remaining, 64 * 1024))
            if not raw:
                return None
            check.update(raw)
            remaining -= len(raw)
        if remaining or check.digest() != digest.digest():
            return None
        return stream.tell()
    return None


def accepted_turns(home, thread_id, keys):
    """Return proven accepted inputs. Missing entries never prove rejection."""
    if (not isinstance(thread_id, str) or not thread_id or not isinstance(keys, list)
            or not keys or len(keys) > 32
            or any(not isinstance(key, str) or not key for key in keys)
            or len(set(keys)) != len(keys)):
        return []
    try:
        home = Path(home).resolve(strict=True)
        deadline = time.monotonic() + 2
        state = _database(home, 'state_5.sqlite', deadline)
        try:
            thread = state.execute('SELECT rollout_path FROM threads WHERE id=?',
                                   (thread_id,)).fetchone()
        finally:
            state.close()
        if not thread:
            return []
        path = Path(thread['rollout_path']).resolve(strict=True)
        relative = path.relative_to(home)
        if relative.parts[0] not in {'sessions', 'archived_sessions'} or path.suffix != '.jsonl':
            return []
        db = _database(home, 'thread_history_1.sqlite', deadline)
        try:
            projection = db.execute('SELECT * FROM thread_history_projection_state WHERE thread_id=?',
                                    (thread_id,)).fetchone()
            if not projection:
                return []
            results, budget = [], {'remaining': SCAN_LIMIT}
            with _open_rollout(home, relative) as stream:
                identity = os.fstat(stream.fileno())
                header = stream.readline(READ_LIMIT + 1)
                if len(header) > READ_LIMIT or not header.endswith(b'\n'):
                    return []
                metadata = json.loads(header)
                if (metadata.get('type') != 'session_meta'
                        or metadata.get('payload', {}).get('id') != thread_id):
                    return []
                for key in keys:
                    if time.monotonic() >= deadline or budget['remaining'] <= 0:
                        break
                    rows = db.execute("SELECT i.item_json,i.item_id,i.turn_id,"
                        "i.rollout_ordinal AS item_ordinal,t.rollout_ordinal AS turn_ordinal,"
                        "t.rollout_byte_offset FROM thread_items i JOIN thread_turns t "
                        "ON t.thread_id=i.thread_id AND t.turn_id=i.turn_id "
                        "WHERE i.thread_id=? AND i.item_type='userMessage' "
                        "AND json_extract(i.item_json,'$.clientId')=? LIMIT 2", (thread_id, key)).fetchall()
                    if len(rows) != 1:
                        continue
                    row = rows[0]
                    item = json.loads(row['item_json'])
                    if (item.get('type') != 'userMessage' or item.get('id') != row['item_id']
                            or item.get('clientId') != key or not isinstance(item.get('content'), list)
                            or not isinstance(row['turn_id'], str) or not row['turn_id']):
                        continue
                    end = _saved_item(stream, thread_id, row, item, deadline, budget)
                    if (end is None or projection['next_rollout_byte_offset'] < end
                            or projection['next_rollout_ordinal'] <= row['item_ordinal']):
                        continue
                    results.append({'id': row['turn_id'], 'startOutcome': 'accepted',
                                    'clientUserMessageId': key, 'items': [item]})
                with _open_rollout(home, relative) as current_stream:
                    current = os.fstat(current_stream.fileno())
                stream.seek(0)
                if ((identity.st_dev, identity.st_ino) != (current.st_dev, current.st_ino)
                        or stream.readline(READ_LIMIT + 1) != header):
                    return []
            return results
        finally:
            db.close()
    except (OSError, sqlite3.Error, ValueError, TypeError, KeyError):
        return []
