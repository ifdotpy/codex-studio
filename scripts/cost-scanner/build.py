#!/usr/bin/env python3
"""Build the isolated Codex log scanner without downloading dependencies."""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import platform
import shutil
import signal
import subprocess
import sys
import tempfile


BUILD_FORMAT_VERSION = "1"
ROOT = Path(__file__).resolve().parent
VENDOR = ROOT / "vendor" / "CodexBarCore"


def source_files():
    files = [ROOT / "main.swift", ROOT / "Support.swift"]
    files.extend(
        [
            VENDOR / "CostUsageModels.swift",
            VENDOR / "Generated" / "CodexParserHash.generated.swift",
        ]
    )
    files.extend(sorted((VENDOR / "Vendored" / "CostUsage").glob("*.swift")))
    if not all(path.is_file() for path in files):
        missing = [str(path) for path in files if not path.is_file()]
        raise RuntimeError("Missing vendored scanner source: " + ", ".join(missing))
    return files


def compiler():
    path = shutil.which("swiftc")
    if path:
        return path
    for candidate in (
        "/usr/bin/swiftc",
        "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc",
    ):
        if os.access(candidate, os.X_OK):
            return candidate
    raise RuntimeError("swiftc is required to build the local cost scanner.")


def fingerprint(files, swiftc):
    digest = hashlib.sha256()
    digest.update(BUILD_FORMAT_VERSION.encode())
    digest.update(platform.system().encode())
    digest.update(platform.machine().encode())
    digest.update(str(Path(swiftc).resolve()).encode())
    version = subprocess.run(
        [swiftc, "--version"],
        check=False,
        capture_output=True,
        text=True,
    )
    digest.update(version.stdout.encode())
    digest.update(version.stderr.encode())
    for path in files:
        digest.update(str(path.relative_to(ROOT)).encode())
        digest.update(path.read_bytes())
    return digest.hexdigest()


def stop_process_group(process):
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def compile_binary(output, files, swiftc):
    fd, temporary_name = tempfile.mkstemp(
        prefix=f".{output.name}.", suffix=".tmp", dir=output.parent
    )
    os.close(fd)
    temporary = Path(temporary_name)
    command = [
        swiftc,
        "-Osize",
        *map(str, files),
        "-o",
        str(temporary),
        "-lsqlite3",
    ]
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
        # The host shell can export the Command Line Tools SDK. It is built
        # by a newer Swift release on this machine than the selected Xcode
        # compiler, so let swiftc select its matching SDK.
        env={key: value for key, value in os.environ.items() if key != "SDKROOT"},
    )
    try:
        stdout, stderr = process.communicate(timeout=110)
    except subprocess.TimeoutExpired:
        stop_process_group(process)
        temporary.unlink(missing_ok=True)
        raise RuntimeError("swiftc timed out while building the local cost scanner.") from None
    if process.returncode:
        temporary.unlink(missing_ok=True)
        detail = (stderr or stdout).strip()
        raise RuntimeError(f"swiftc failed: {detail[-4000:]}")
    os.chmod(temporary, 0o700)
    os.replace(temporary, output)


def main():
    if platform.system() != "Darwin":
        raise RuntimeError("The local cost scanner requires macOS.")
    if len(sys.argv) != 3 or sys.argv[1] != "--output":
        raise RuntimeError("usage: build.py --output ABSOLUTE_OUTPUT")
    output = Path(sys.argv[2])
    if not output.is_absolute():
        raise RuntimeError("--output must be an absolute path")
    if output.exists() and output.is_dir():
        raise RuntimeError("--output must name a file")
    output.parent.mkdir(parents=True, exist_ok=True)

    files = source_files()
    swiftc = compiler()
    value = fingerprint(files, swiftc)
    marker = output.with_name(output.name + ".fingerprint")
    if output.is_file() and marker.is_file():
        try:
            if marker.read_text().strip() == value:
                os.chmod(output, 0o700)
                return
        except OSError:
            pass

    compile_binary(output, files, swiftc)
    temporary_marker = marker.with_name(f".{marker.name}.tmp")
    temporary_marker.write_text(value + "\n")
    os.chmod(temporary_marker, 0o600)
    os.replace(temporary_marker, marker)


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1) from error
