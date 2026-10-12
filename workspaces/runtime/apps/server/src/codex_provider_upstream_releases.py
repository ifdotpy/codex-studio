"""Cache unauthenticated upstream npm release metadata for provider CLIs."""
from __future__ import annotations

import json
import logging
import os
from pathlib import Path
import sqlite3
import tempfile
import threading
import time
from collections.abc import Callable, Mapping
from urllib.error import HTTPError
from urllib.request import Request

from codex_api_client import open_request
from codex_cache_paths import cache_dir


CHECK_INTERVAL_SECONDS = 24 * 60 * 60
REQUEST_TIMEOUT_SECONDS = 5
ACCEPT_HEADER = "application/vnd.npm.install-v1+json"
PREFERENCE_KEY = '["codex-studio-preferences-v1","checkProviderUpdates"]'
PACKAGES = {
    "claude": (
        "@anthropic-ai/claude-code",
        "https://registry.npmjs.org/@anthropic-ai%2fclaude-code",
    ),
    "codex": (
        "@openai/codex",
        "https://registry.npmjs.org/@openai%2fcodex",
    ),
}
_LOG = logging.getLogger(__name__)

RegistryRequest = Callable[
    [str, str | None, float], tuple[int, Mapping[str, str], bytes]
]


def _registry_request(
    url: str, etag: str | None, timeout: float
) -> tuple[int, Mapping[str, str], bytes]:
    headers = {"Accept": ACCEPT_HEADER}
    if etag:
        headers["If-None-Match"] = etag
    request = Request(url, headers=headers)
    try:
        with open_request(request, timeout=timeout) as response:
            return response.status, dict(response.headers.items()), response.read()
    except HTTPError as error:
        if error.code != 304:
            raise
        response_headers = dict(error.headers.items()) if error.headers else {}
        error.close()
        return 304, response_headers, b""


class UpstreamReleaseChecker:
    """Fetch and persist each package's latest tag at most once per day."""

    def __init__(
        self,
        cache_path: Path | None = None,
        *,
        request: RegistryRequest | None = None,
        clock: Callable[[], float] = time.time,
    ) -> None:
        self.path = cache_path or cache_dir() / "provider-upstream-releases-v1.json"
        self.request = request or _registry_request
        self.clock = clock
        self.lock = threading.Lock()
        self.cache = self._read()

    def _read(self) -> dict[str, dict[str, object]]:
        try:
            value = json.loads(self.path.read_text(encoding="utf-8"))
            packages = value.get("packages") if isinstance(value, dict) else None
            if isinstance(packages, dict):
                return {
                    name: entry
                    for name, entry in packages.items()
                    if name in {package for package, _url in PACKAGES.values()}
                    and isinstance(entry, dict)
                }
        except (OSError, UnicodeDecodeError, ValueError, TypeError):
            pass
        return {}

    def _save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        encoded = json.dumps({"version": 1, "packages": self.cache}, separators=(",", ":"))
        with tempfile.NamedTemporaryFile(
            mode="w", encoding="utf-8", dir=self.path.parent,
            prefix=".provider-upstream-releases-", delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
            temporary.write(encoded)
        try:
            os.chmod(temporary_path, 0o600)
            os.replace(temporary_path, self.path)
        finally:
            temporary_path.unlink(missing_ok=True)

    @staticmethod
    def _header(headers: Mapping[str, str], name: str) -> str | None:
        return next((value for key, value in headers.items() if key.lower() == name.lower()), None)

    def _cached_version(self, entry: dict[str, object], now: float) -> str | None:
        version = entry.get("version")
        fetched_at = entry.get("fetchedAt")
        if (isinstance(version, str) and isinstance(fetched_at, (int, float))
                and now - fetched_at < CHECK_INTERVAL_SECONDS):
            from codex_provider_update_notice import parse_version
            return version if parse_version(version) is not None else None
        return None

    @staticmethod
    def _valid_version(entry: dict[str, object]) -> str | None:
        version = entry.get("version")
        if not isinstance(version, str):
            return None
        from codex_provider_update_notice import parse_version
        return version if parse_version(version) is not None else None

    def check(self, *, enabled: bool = True) -> dict[str, str]:
        """Return valid cached/latest versions; disabled or failed sources return none."""
        if not enabled:
            return {}
        latest: dict[str, str] = {}
        with self.lock:
            now = self.clock()
            for provider, (package, url) in PACKAGES.items():
                entry = self.cache.setdefault(package, {})
                last_attempt = entry.get("lastAttemptAt", 0)
                if isinstance(last_attempt, (int, float)) and now - last_attempt < CHECK_INTERVAL_SECONDS:
                    cached = self._cached_version(entry, now)
                    if cached is not None:
                        latest[provider] = cached
                    continue

                entry["lastAttemptAt"] = now
                try:
                    etag = entry.get("etag")
                    status, headers, body = self.request(
                        url, etag if isinstance(etag, str) else None,
                        REQUEST_TIMEOUT_SECONDS,
                    )
                    if status == 304:
                        version = self._valid_version(entry)
                        if version is None:
                            raise ValueError("Registry returned 304 without a valid cached version")
                        entry["fetchedAt"] = now
                        new_etag = self._header(headers, "ETag")
                        if new_etag:
                            entry["etag"] = new_etag
                    elif status == 200:
                        document = json.loads(body)
                        tags = document.get("dist-tags") if isinstance(document, dict) else None
                        version = tags.get("latest") if isinstance(tags, dict) else None
                        if not isinstance(version, str):
                            raise ValueError("Registry metadata has no latest dist-tag")
                        from codex_provider_update_notice import parse_version
                        if parse_version(version) is None:
                            raise ValueError("Registry latest dist-tag is not a recognized version")
                        entry["version"] = version
                        entry["fetchedAt"] = now
                        new_etag = self._header(headers, "ETag")
                        if new_etag:
                            entry["etag"] = new_etag
                    else:
                        raise ValueError(f"Registry returned HTTP {status}")
                    self._save()
                    latest[provider] = version
                except (OSError, TimeoutError, UnicodeDecodeError, ValueError, TypeError) as error:
                    _LOG.warning("Could not check upstream release for %s: %s", package, error)
                    try:
                        self._save()
                    except OSError:
                        pass
        return latest


def updates_enabled(runtime: object) -> bool | None:
    read_db = getattr(runtime, "read_db", None)
    if not callable(read_db):
        return None
    try:
        from studio_api.sync.preferences import read_preferences
        with read_db() as db:
            fields = read_preferences(db, "user").fields
        preference = fields.get(PREFERENCE_KEY)
        return preference.value is not False if preference is not None else True
    except (OSError, sqlite3.Error, ValueError) as error:
        _LOG.warning("Could not read provider update preference: %s", error)
        return False


_CHECKER_LOCK = threading.Lock()
_CHECKER: UpstreamReleaseChecker | None = None


def tick(runtime: object) -> dict[str, str]:
    """Check on the provider monitor's background worker, never a request thread."""
    enabled = updates_enabled(runtime)
    if enabled is not True:
        return {}
    global _CHECKER
    if _CHECKER is None:
        with _CHECKER_LOCK:
            if _CHECKER is None:
                _CHECKER = UpstreamReleaseChecker()
    return _CHECKER.check()
