"""Rosetta for Linux in the guest (docs/linux-vm-workspaces.md, "x86_64 binaries").

The Mac helper attaches Apple's Rosetta directory share when the Mac has Rosetta. This module
mounts it, registers it as the binfmt_misc handler of x86_64 ELF files, and installs the small
amd64 runtime that dynamic x86_64 binaries need. Without the share every step is a no-op, so
a Mac without Rosetta keeps a working VM.

`activate` runs at each boot (codex-studio-rosetta.service). `install-runtime` runs from
install.sh, so a fresh provision and an in-place guest update both install the runtime.
"""
from __future__ import annotations

import os
from pathlib import Path
import re
import subprocess
import sys

from shared_values_generated import LINUX_VM_ROSETTA_TAG

MOUNT = Path("/media/rosetta")
BINFMT = Path("/proc/sys/fs/binfmt_misc")
HANDLER = "rosetta"
# Apple's documented x86_64 ELF match ("Running Intel Binaries in Linux VMs with Rosetta"):
# a 64-bit little-endian ELF header with e_type ET_EXEC or ET_DYN and e_machine EM_X86_64.
MAGIC = r"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00"
MASK = r"\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff"
# F (fix binary): the kernel opens the interpreter once, at registration, so x86_64 programs
# also start in other mount namespaces (the Codex sandbox, layr lines). No C or O: a set-user-ID
# x86_64 file runs with the caller's credentials, which normal binaries do not need.
FLAGS = "F"
# The runtime of a dynamic x86_64 glibc program: the loader and libc, libstdc++ (with its
# dependency libgcc-s1) and zlib. All are in Ubuntu main.
AMD64_PACKAGES = ("libc6:amd64", "libstdc++6:amd64", "zlib1g:amd64")
# ports.ubuntu.com, the arm64 mirror, has no amd64 packages. apt skips it for amd64 with a
# notice, so the image's sources stay unchanged.
SOURCES = Path("/etc/apt/sources.list.d/codex-studio-amd64.sources")
KEYRING = "/usr/share/keyrings/ubuntu-archive-keyring.gpg"
UPDATE_SECONDS = 300
INSTALL_SECONDS = 600


def log(message: str) -> None:
    print("STUDIO_ROSETTA: " + message, flush=True)


def binfmt_rule(interpreter: Path = MOUNT / "rosetta") -> str:
    """The binfmt_misc register line: :name:type:offset:magic:mask:interpreter:flags."""
    return f":{HANDLER}:M::{MAGIC}:{MASK}:{interpreter}:{FLAGS}"


def amd64_sources(codename: str) -> str:
    if not re.fullmatch(r"[a-z]+", codename):
        raise ValueError("The Ubuntu release codename is invalid")
    stanza = ("Types: deb\nURIs: {uri}\nSuites: {suites}\nComponents: main\n"
              "Architectures: amd64\nSigned-By: " + KEYRING + "\n")
    return (stanza.format(uri="http://archive.ubuntu.com/ubuntu", suites=f"{codename} {codename}-updates")
            + "\n" + stanza.format(uri="http://security.ubuntu.com/ubuntu", suites=f"{codename}-security"))


def codename(os_release: Path = Path("/etc/os-release")) -> str:
    for line in os_release.read_text().splitlines():
        if line.startswith("VERSION_CODENAME="):
            return line.split("=", 1)[1].strip().strip('"')
    raise ValueError("The Ubuntu release codename is unavailable")


def run(argv: list[str], timeout: float) -> subprocess.CompletedProcess:
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout,
                          env={**os.environ, "DEBIAN_FRONTEND": "noninteractive"})


def registered(binfmt: Path, interpreter: Path) -> bool:
    """Whether the handler is enabled with this interpreter and these flags."""
    try:
        lines = (binfmt / HANDLER).read_text().splitlines()
    except FileNotFoundError:
        return False
    return ("enabled" in lines and f"interpreter {interpreter}" in lines
            and f"flags: {FLAGS}" in lines)


def activate(*, mount: Path = MOUNT, binfmt: Path = BINFMT, runner=run) -> int:
    """Mount the share and register the handler. Exit 0 without the share."""
    interpreter = mount / "rosetta"
    if not os.path.ismount(mount):
        mount.mkdir(parents=True, exist_ok=True)
        result = runner(["mount", "-t", "virtiofs", "-o", "ro", LINUX_VM_ROSETTA_TAG, str(mount)], 30)
        if result.returncode != 0:
            log(f"no virtiofs share '{LINUX_VM_ROSETTA_TAG}'; x86_64 binaries are unavailable")
            return 0
    if not os.access(interpreter, os.X_OK):
        log(f"the share has no executable {interpreter}")
        return 1
    if not (binfmt / "register").exists():
        result = runner(["mount", "-t", "binfmt_misc", "binfmt_misc", str(binfmt)], 30)
        if result.returncode != 0:
            log("cannot mount binfmt_misc: " + result.stderr.strip()[-500:])
            return 1
    if registered(binfmt, interpreter):
        log("ready")
        return 0
    if (binfmt / HANDLER).exists():
        # A handler with another interpreter or other flags: replace it.
        (binfmt / HANDLER).write_text("-1")
    (binfmt / "register").write_text(binfmt_rule(interpreter))
    log("ready")
    return 0


def installed(package: str, runner=run) -> bool:
    result = runner(["dpkg-query", "-W", "-f=${Status}", package], 30)
    return result.returncode == 0 and result.stdout.strip() == "install ok installed"


def install_runtime(*, sources: Path = SOURCES, os_release: Path = Path("/etc/os-release"), runner=run) -> int:
    """Add the amd64 architecture and install AMD64_PACKAGES once. Network use only when a
    package is missing."""
    if all(installed(package, runner) for package in AMD64_PACKAGES):
        return 0
    text = amd64_sources(codename(os_release))
    if not sources.exists() or sources.read_text() != text:
        sources.write_text(text)
    sources.chmod(0o644)
    steps = [(["dpkg", "--add-architecture", "amd64"], 30),
             (["apt-get", "update"], UPDATE_SECONDS),
             (["apt-get", "install", "-y", "--no-install-recommends", "-o", "DPkg::Lock::Timeout=300",
               *AMD64_PACKAGES], INSTALL_SECONDS)]
    for argv, timeout in steps:
        try:
            result = runner(argv, timeout)
        except subprocess.TimeoutExpired:
            log(f"{argv[0]} {argv[1]} exceeded {timeout} seconds")
            return 1
        if result.returncode != 0:
            log(f"{argv[0]} {argv[1]} failed: " + (result.stderr or result.stdout).strip()[-500:])
            return 1
    log("amd64 runtime installed")
    return 0


def main(argv: list[str]) -> int:
    # The root broker runs install.sh with UMask=0077. apt lists, the sources file and the
    # mount point must stay readable by every user.
    os.umask(0o022)
    if argv == ["activate"]:
        return activate()
    if argv == ["install-runtime"]:
        return install_runtime()
    print("Use rosetta.py activate or rosetta.py install-runtime.", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
