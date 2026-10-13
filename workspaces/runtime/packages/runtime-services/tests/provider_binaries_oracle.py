#!/usr/bin/env python3
"""Run provider-version oracle functions against a shared JSON corpus."""

import json
import sys
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).absolute().parent / "turn_lifecycle"))
from oracle_guard import install_guard

_GUARD = install_guard(block_processes=True)

server_source = Path(sys.argv[1])
corpus_path = Path(sys.argv[2])
sys.path.insert(0, str(server_source))

from codex_provider_update_notice import compare_versions, parse_version
from codex_provider_versions import _diagnostic, version_warning
from codex_native_binary import bundle_digest, version_key

corpus = json.loads(corpus_path.read_text(encoding="utf-8"))
parsed = []
for value in corpus["versions"]:
    parsed_value = parse_version(value)
    parsed.append(
        None
        if parsed_value is None
        else [
            parsed_value[0],
            parsed_value[1],
            parsed_value[2],
            parsed_value[3],
            [[part[0], part[1]] for part in parsed_value[4]],
        ]
    )

comparisons = []
for case in corpus["comparisons"]:
    notice = compare_versions(
        case["installed"], case["available"], source=case["source"]
    )
    comparisons.append(None if notice is None else notice.model_dump(by_alias=True))

diagnostics = []
for case in corpus["diagnostics"]:
    versions = {
        "runningVersion": case["runningVersion"],
        "installedVersion": case["installedVersion"],
        "configuredVersion": case["configuredVersion"],
        "availableVersion": case["availableVersion"],
        "upstreamVersion": case["upstreamVersion"],
        "error": case["error"],
        "runningNote": case["runningNote"],
    }
    with patch("codex_provider_versions.time.time", return_value=case["at"]):
        diagnostics.append(
            _diagnostic(case["accountKey"], case["provider"], versions)
        )

warnings = []
for case in corpus["diagnostics"]:
    if case["runningVersion"]:
        warning = version_warning(case["provider"], case["runningVersion"])
    else:
        warning = None
    warnings.append(warning)

native_versions = []
for value in corpus["nativeVersions"]:
    try:
        native_versions.append(version_key(value))
    except ValueError:
        native_versions.append(None)

bundle_hash = bundle_digest(dict(corpus["hashes"]))

print(
    json.dumps(
        {
            "parsed": parsed,
            "comparisons": comparisons,
            "diagnostics": diagnostics,
            "warnings": warnings,
            "nativeVersions": native_versions,
            "bundleDigest": bundle_hash,
        },
        separators=(",", ":"),
    )
)
