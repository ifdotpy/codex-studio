"""Python `codex_server_exec` output pages, marker redaction and request checks.

The Rust test feeds the same inputs to its port and compares every result.
No command runs.
"""
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps/server/src"))
from codex_server_exec import Buffer, MarkerRedactor, validate

buffer_cases = []
for limit, chunks, reads in [
    (6, [b"abcdefghij"], [(0, 32, True), (3, 32, True), (7, 2, True), (10, 4, True)]),
    (100, ["\u00e9".encode(), b"\xe2\x82"], [(0, 32, False), (0, 32, True), (2, 32, False)]),
    (8, ["\u00e9\u00e9\u00e9\u00e9\u00e9".encode()], [(0, 32, True), (2, 32, True), (4, 32, False), (4, 32, True)]),
    (4, ["\u20ac\u20ac".encode()], [(0, 1, True), (0, 32, False), (1, 32, True)]),
    (3, [b"\xff\xfeab", b"c"], [(0, 32, True), (1, 32, True)]),
    (10, [], [(0, 32, True)]),
]:
    buffer = Buffer(limit)
    for chunk in chunks:
        buffer.append(chunk)
    buffer_cases.append({"limit": limit, "chunks": [chunk.hex() for chunk in chunks],
                         "head": buffer.head.hex(), "tail": buffer.tail.hex(), "total": buffer.total,
                         "reads": [{"offset": o, "limit": n, "final": f, "page": buffer.read(o, n, final=f)}
                                   for o, n, f in reads]})

redaction_cases = []
for secret, chunks in [
    ("secret", [b"a sec", b"ret b", b""]),
    ("secret", [b"sec", b""]),
    ("secret", [b"secretsecret", b"s", b"ecre", b"t!", b""]),
    ("abc", [b"aab", b"cab", b"c", b""]),
]:
    redactor = MarkerRedactor(secret)
    out = [redactor.feed(chunk, final=index == len(chunks) - 1).hex() for index, chunk in enumerate(chunks)]
    redaction_cases.append({"secret": secret, "chunks": [chunk.hex() for chunk in chunks], "out": out})

validate_cases = []
for payload, foreign in [
    ({"cwd": "/tmp", "command": ["true"]}, False),
    ({"cwd": "/tmp", "command": "echo hi", "env": {"A_B": "1"}, "timeout": 30, "output_limit": 2}, False),
    ({"cwd": "relative", "command": ["true"]}, False),
    ({"cwd": "C:\\work", "command": ["true"]}, False),
    ({"cwd": "C:\\work", "command": ["true"]}, True),
    ({"cwd": "\\\\host\\share\\dir", "command": ["true"]}, True),
    ({"cwd": "/tmp/../etc", "command": ["true"]}, False),
    ({"cwd": "/tmp", "command": ""}, False),
    ({"cwd": "/tmp", "command": []}, False),
    ({"cwd": "/tmp", "command": [""]}, False),
    ({"cwd": "/tmp", "command": ["a\u0000"]}, False),
    ({"cwd": "/tmp", "command": ["true"], "env": {"1A": "x"}}, False),
    ({"cwd": "/tmp", "command": ["true"], "env": {"A": 1}}, False),
    ({"cwd": "/tmp", "command": ["true"], "timeout": 0}, False),
    ({"cwd": "/tmp", "command": ["true"], "timeout": 1801}, False),
    ({"cwd": "/tmp", "command": ["true"], "timeout": True}, False),
    ({"cwd": "/tmp", "command": ["true"], "timeout": 2.0}, False),
    ({"cwd": "/tmp", "command": ["true"], "output_limit": 1}, False),
    ({"cwd": "/tmp", "command": ["true"], "output_limit": 4194305}, False),
    ({"cwd": 5, "command": ["true"]}, False),
]:
    try:
        validate_cases.append({"payload": payload, "foreign": foreign, "value": validate(payload, allow_foreign_windows_path=foreign)})
    except ValueError as error:
        validate_cases.append({"payload": payload, "foreign": foreign, "error": str(error)})

print(json.dumps({"buffers": buffer_cases, "redactions": redaction_cases, "validate": validate_cases}))
