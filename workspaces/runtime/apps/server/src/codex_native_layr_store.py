"""Persistent case-sensitive APFS storage for native Layr on macOS.

The original project stays outside this image. This module never replaces an
image or detaches a mounted store.
"""
from __future__ import annotations

import json
from pathlib import Path
import plistlib
import subprocess

from codex_file_lock import LOCK_EX, LOCK_UN, flock
from codex_native_layr import Client

# Virtual capacity of a sparse native store, not an immediate allocation.
NATIVE_STORE_CAPACITY = "200g"


def run(*args: str, timeout: float = 120) -> bytes:
    result = subprocess.run(args, check=True, capture_output=True, timeout=timeout)
    return result.stdout


def mounted_image(image: Path, mount: Path) -> bool:
    info = plistlib.loads(run("/usr/bin/hdiutil", "info", "-plist"))
    for item in info.get("images", []):
        locations = [Path(e["mount-point"]).resolve() for e in item.get("system-entities", [])
                     if e.get("mount-point")]
        same_image = Path(item.get("image-path", "")).resolve() == image
        if mount in locations:
            if not same_image:
                raise RuntimeError("The native Layr mount contains another image")
            return True
        if same_image and locations:
            raise RuntimeError("The native Layr image is mounted at another path")
    if mount.is_mount():
        raise RuntimeError("The native Layr mount contains another volume")
    return False


def ensure_macos(root: Path) -> Path:
    root = root.resolve()
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    image = root / "history.sparsebundle"
    mount = root / "volume"
    receipt = root / "store.json"
    with (root / "store.lock").open("a+b") as lock:
        flock(lock, LOCK_EX)
        try:
            if image.is_symlink() or mount.is_symlink():
                raise ValueError("Native Layr storage paths must not be symbolic links")
            record = json.loads(receipt.read_text()) if receipt.exists() else None
            if record is None:
                if image.exists() or mount.exists():
                    raise RuntimeError("Native Layr storage exists without its creation receipt")
                record = {"image": str(image), "mount": str(mount), "phase": "creating"}
                Client.write(receipt, record)
                run("/usr/bin/hdiutil", "create", "-size", NATIVE_STORE_CAPACITY,
                    "-type", "SPARSEBUNDLE", "-fs", "Case-sensitive APFS",
                    "-volname", "LayrNative", str(image), timeout=900)
                record["phase"] = "created"
                Client.write(receipt, record)
            if record.get("image") != str(image) or record.get("mount") != str(mount):
                raise RuntimeError("The native Layr store identity changed")
            if record.get("phase") not in {"created", "ready"}:
                raise RuntimeError("Native Layr image creation has an unknown outcome; inspect the image")
            if not image.is_dir():
                raise RuntimeError("The native Layr image is missing")
            if not mounted_image(image, mount):
                mount.mkdir(mode=0o700, exist_ok=True)
                if any(mount.iterdir()):
                    raise RuntimeError("The native Layr mount folder is not empty")
                run("/usr/bin/hdiutil", "attach", "-nobrowse", "-owners", "on",
                    "-mountpoint", str(mount), str(image), timeout=300)
                if not mounted_image(image, mount):
                    raise RuntimeError("The native Layr image did not mount at its expected path")
            info = plistlib.loads(run("/usr/sbin/diskutil", "info", "-plist", str(mount)))
            if (info.get("FilesystemType") != "apfs"
                    or info.get("FilesystemName") != "Case-sensitive APFS"
                    or info.get("GlobalPermissionsEnabled") is not True):
                raise RuntimeError("Native Layr needs case-sensitive APFS with ownership enabled")
            volume = info.get("VolumeUUID")
            if not isinstance(volume, str) or not volume:
                raise RuntimeError("The native Layr volume has no identity")
            if record.get("volume") and record["volume"] != volume:
                raise RuntimeError("The native Layr volume identity changed")
            record.update(phase="ready", volume=volume)
            Client.write(receipt, record)
            return mount / "store"
        finally:
            flock(lock, LOCK_UN)
