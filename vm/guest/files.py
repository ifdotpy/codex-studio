"""Read bounded file chunks, with identity checks, as the line owner."""
import base64
import hashlib
import json
import os
from pathlib import Path
import stat
import sys

from common import MAX_INPUT, GuestError, integer, require


def token(info):
    return hashlib.sha256(f"{info.st_dev}:{info.st_ino}:{info.st_size}:{info.st_mtime_ns}:{info.st_ctime_ns}".encode()).hexdigest()


try:
    request = json.load(sys.stdin)
    path = Path(request["path"]).resolve()
    root = Path(request["allowedRoot"]).resolve()
    require(path.is_relative_to(root), "The file path is outside its permitted root")
    require(stat.S_ISREG(path.stat().st_mode), "The path must identify a regular file")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as source:
        info = os.fstat(source.fileno())
        require(stat.S_ISREG(info.st_mode), "The path must identify a regular file")
        identity = token(info)
        if request["method"] == "file.stat":
            digest = hashlib.file_digest(source, "sha256").hexdigest()
            require(token(os.fstat(source.fileno())) == identity, "The file changed during the checksum check")
            result = {"path": str(path), "bytes": info.st_size, "token": identity, "sha256": digest}
        else:
            require(request.get("token") == identity, "The file changed since file.stat")
            offset = integer(request.get("offset"), 0, 0, info.st_size)
            limit = integer(request.get("maxBytes"), 256 * 1024, 1, MAX_INPUT)
            source.seek(offset)
            data = source.read(limit)
            require(token(os.fstat(source.fileno())) == identity, "The file changed during the read")
            result = {"path": str(path), "offset": offset, "nextOffset": offset + len(data),
                      "bytes": info.st_size, "token": identity, "data": base64.b64encode(data).decode(),
                      "sha256": hashlib.sha256(data).hexdigest()}
    print(json.dumps({"result": result}))
except GuestError as exc:
    print(json.dumps({"error": exc.object()}))
    sys.exit(1)
except Exception:
    print(json.dumps({"error": {"code": "invalid_params", "message": "The guest file is unavailable"}}))
    sys.exit(1)
