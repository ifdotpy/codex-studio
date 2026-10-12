"""Compare provider executables and describe newer installed versions."""
from __future__ import annotations

import re

from studio_api.system.models import ProviderUpdateNotice


VERSION_PATTERN = re.compile(
    r"(?<![0-9])([0-9]+)\.([0-9]+)\.([0-9]+)(?:-([0-9A-Za-z.-]+))?"
)
VersionKey = tuple[int, int, int, bool, tuple[tuple[int, int | str], ...]]


def parse_version(value: object) -> VersionKey | None:
    if not isinstance(value, str):
        return None
    match = VERSION_PATTERN.search(value)
    if not match:
        return None
    prerelease = match[4]
    parts: tuple[tuple[int, int | str], ...] = tuple(
        (0, int(part)) if part.isdigit() else (1, part)
        for part in (prerelease or "").split(".")
    )
    return (
        int(match[1]),
        int(match[2]),
        int(match[3]),
        prerelease is None,
        parts,
    )


def compare_versions(
    provider: str,
    installed_version: str | None,
    available_version: str | None,
) -> ProviderUpdateNotice | None:
    """Return a notice only when a readable installed candidate is newer."""
    if provider not in {"codex", "claude"}:
        return None
    if installed_version is None or available_version is None:
        return None
    installed = parse_version(installed_version)
    available = parse_version(available_version)
    if installed is None or available is None or available <= installed:
        return None
    return ProviderUpdateNotice(availableVersion=available_version, source="installed")
