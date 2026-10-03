"""Redacted provider JSON-RPC transcript capture for opt-in diagnostics."""
import json
import os
from pathlib import Path
import re
import threading
import time


_SECRET_TEXT = re.compile(
    r"(?i)(\bBearer\s+|(?:access[_-]?token|refresh[_-]?token|api[_-]?key|password|secret|token)\s*[=:]\s*)"
    r"[A-Za-z0-9._~+/=-]+|\b(eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}|"
    r"sk-[A-Za-z0-9_-]{12,}|gh[pousr]_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{12,})"
)
_PATH_TEXT = re.compile(r"(?<![A-Za-z0-9:/])/(?:[A-Za-z0-9._~+-]+/)+[A-Za-z0-9._~+-]*")


def redact(value, key=""):
    normalized_key = re.sub(r"[^a-z0-9]", "", key.lower())
    sensitive_key = (normalized_key in {"token", "tokens", "authorization", "credentials", "credential", "secret", "password", "cookie"}
                     or normalized_key.endswith(("apikey", "accesstoken", "refreshtoken", "idtoken")))
    if sensitive_key:
        return "<REDACTED>"
    if isinstance(value, dict):
        return {str(k): redact(v, str(k)) for k, v in value.items()}
    if isinstance(value, list):
        return [redact(item) for item in value]
    if isinstance(value, str):
        path_key = normalized_key in {"path", "cwd", "workdir", "directory", "filename", "filepath", "file", "homedir", "root"}
        if path_key and (value.startswith(("/", "\\\\")) or re.match(r"^[A-Za-z]:[\\\\/]", value)):
            return "<PATH>"
        value = _SECRET_TEXT.sub(lambda match: (match.group(1) or "") + "<REDACTED>", value)
        return _PATH_TEXT.sub("<PATH>", value)
    return value


class TranscriptCapture:
    """Append one redacted JSON-RPC frame per line when capture is enabled."""

    def __init__(self, provider):
        enabled = os.environ.get("CODEX_AGENTS_PROVIDER_CAPTURE") == "1"
        filename = os.environ.get("CODEX_AGENTS_PROVIDER_CAPTURE_FILE")
        self.path = Path(filename).expanduser() if enabled and filename else None
        self.provider = provider
        self.lock = threading.Lock()

    def record(self, direction, message):
        if self.path is None:
            return
        row = {"timestamp": time.time(), "provider": self.provider,
               "direction": direction, "message": redact(message)}
        encoded = json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n"
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with self.lock, self.path.open("a", encoding="utf-8") as stream:
                stream.write(encoded)
                stream.flush()
        except OSError:
            # Capture must not change provider request or response handling.
            return
