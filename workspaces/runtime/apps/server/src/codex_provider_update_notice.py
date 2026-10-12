"""Compare provider executables and describe newer installed versions."""
from __future__ import annotations

import re
from typing import Any, Literal

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
    installed_version: str | None,
    available_version: str | None,
    *,
    source: Literal["installed", "upstream"] = "installed",
) -> ProviderUpdateNotice | None:
    """Return a notice only when a readable installed candidate is newer."""
    if installed_version is None or available_version is None:
        return None
    installed = parse_version(installed_version)
    available = parse_version(available_version)
    if installed is None or available is None or available <= installed:
        return None
    return ProviderUpdateNotice(availableVersion=available_version, source=source)


def update_warnings(
    providers: list[dict[str, Any]], labels: dict[str, str]
) -> list[dict[str, Any]]:
    """Project installed update notices into the existing informational tray."""
    warnings = []
    for row in providers:
        notices = row.get("updateNotices", [])
        if not notices:
            continue
        current = row.get("installedVersion") or "unknown"
        label = labels.get(row["provider"], row["provider"])
        for notice in notices:
            available = notice["availableVersion"]
            source = notice["source"]
            if source == "upstream":
                message = (
                    f"A newer {label} release is available upstream than the installed version. "
                    f"Installed: {current} · Available: {available}."
                )
            elif row["provider"] == "codex":
                message = (
                    f"A newer installed {label} version is selected for runtime updates, "
                    f"but this connection is still using {current}. Available: {available}."
                )
            else:
                message = (
                    f"A newer installed {label} version is not in use on this connection yet. "
                    f"Current: {current} · Available: {available}."
                )
            warnings.append({
                "id": f"provider-update:{row['accountKey']}" +
                (":upstream" if source == "upstream" else ""),
                "accountKey": row["accountKey"],
                "provider": row["provider"],
                "version": available,
                # Keep this field's established meaning: the tested minimum.
                "baseline": row["baseline"],
                "message": message,
                "at": row["at"],
                "level": "info",
            })
    return warnings
