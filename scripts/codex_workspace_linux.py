"""Linux overlayfs backend for image workspaces."""

from __future__ import annotations

import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any


_GIT_OBJECTS_MARKER = "HEAD"
_HOLDERS: dict[int, subprocess.Popen[bytes]] = {}
_OVERLAY_MOUNT = r'''
import ctypes, os, sys
libc = ctypes.CDLL(None, use_errno=True)
source, target, options = (os.fsencode(item) for item in sys.argv[1:])
if libc.mount(source, target, b"overlay", 0, options) != 0:
    error = ctypes.get_errno()
    raise OSError(error, os.strerror(error), os.fsdecode(target))
'''
_OVERLAY_UNMOUNT = r'''
import ctypes, os, sys
libc = ctypes.CDLL(None, use_errno=True)
target = os.fsencode(sys.argv[1])
if libc.umount2(target, 0) != 0:
    error = ctypes.get_errno()
    raise OSError(error, os.strerror(error), os.fsdecode(target))
'''


def _run(argv: list[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(argv, check=False, text=True, capture_output=True)
    if check and result.returncode:
        raise RuntimeError(f"Command failed ({result.returncode}): {argv!r}: {result.stderr.strip()}")
    return result


def _git_object_stores(root: Path) -> list[Path]:
    """Return object stores that belong to a Git directory or bare repository."""
    stores: list[Path] = []
    for current, dirs, _files in os.walk(root, topdown=True, followlinks=False):
        current_path = Path(current)
        for name in list(dirs):
            if name == "objects" and (current_path / _GIT_OBJECTS_MARKER).is_file():
                stores.append(current_path / name)
                dirs.remove(name)
    return stores


def _copy_without_object_stores(source: Path, destination: Path) -> None:
    """Copy tree in large reflink-capable chunks, skipping Git object stores."""
    source = source.resolve()
    if destination.exists():
        if destination.is_dir() and not destination.is_symlink():
            shutil.rmtree(destination)
        else:
            destination.unlink()
    destination.mkdir(parents=True, exist_ok=True)
    stores = {path.relative_to(source) for path in _git_object_stores(source)}
    blocked = set(stores)
    for store in stores:
        blocked.update(store.parents)
    blocked.discard(Path("."))

    def copy_dir(src: Path, dst: Path, relative: Path) -> None:
        dst.mkdir(parents=True, exist_ok=True)
        for entry in os.scandir(src):
            rel = relative / entry.name
            src_entry = Path(entry.path)
            if rel in stores:
                continue
            if entry.is_dir(follow_symlinks=False) and rel in blocked:
                copy_dir(src_entry, dst / entry.name, rel)
            else:
                _run(["cp", "-a", "--reflink=auto", "--", str(src_entry), str(dst)])

    copy_dir(source, destination, Path("."))


def _namespace_state_path() -> Path:
    store = _workspace_store()
    return store / "linux-namespace.json"


def _workspace_store() -> Path:
    return Path(os.environ.get(
        "CODEX_WORKSPACE_STORE",
        Path.home() / ".local" / "state" / "codex-agents" / "workspaces",
    )).expanduser().resolve()


def _proc_start_time(pid: int) -> str | None:
    try:
        stat_text = Path(f"/proc/{pid}/stat").read_text()
        fields = stat_text[stat_text.rfind(")") + 2:].split()
        if fields[0] in {"Z", "X"}:
            return None
        return fields[19]
    except (FileNotFoundError, IndexError, PermissionError):
        return None


def _namespace_alive(record: dict[str, Any]) -> bool:
    try:
        pid = int(record["pid"])
    except (KeyError, TypeError, ValueError):
        return False
    if _proc_start_time(pid) != str(record.get("startTime")):
        return False
    try:
        return all(Path(f"/proc/{pid}/ns/{name}").stat().st_ino !=
                   Path(f"/proc/self/ns/{name}").stat().st_ino for name in ("user", "mnt"))
    except (FileNotFoundError, PermissionError):
        return False


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_name = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            json.dump(value, output)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temp_name, path)
    finally:
        try:
            os.unlink(temp_name)
        except FileNotFoundError:
            pass


class Backend:
    """Implement the common image workspace backend contract."""

    def supported(self, repo_root: Path) -> tuple[bool, str]:
        if not sys.platform.startswith("linux"):
            return False, "Linux overlay workspaces require Linux"
        missing = [name for name in ("unshare", "nsenter", "rsync", "cp")
                   if shutil.which(name) is None]
        if missing:
            return False, "Missing Linux workspace tools: " + ", ".join(missing)
        if not Path("/proc/self/ns/user").exists():
            return False, "User namespaces are unavailable"
        return True, "Linux overlayfs is available"

    def current_event_id(self, repo_root: Path) -> None:
        del repo_root
        return None

    def open_base_staging(self, repo_root: Path, repo_key: str, version: str) -> dict[str, Any]:
        del repo_root
        parent = _workspace_store() / "bases" / repo_key
        parent.mkdir(parents=True, exist_ok=True)
        staging_path = parent / f".{version}.building"
        if not staging_path.exists():
            if _btrfs_path(parent):
                _run(["btrfs", "subvolume", "create", str(staging_path)])
            else:
                staging_path.mkdir()
        repo = staging_path / "repo"
        repo.mkdir(exist_ok=True)
        (staging_path / "home").mkdir(exist_ok=True)
        (staging_path / "tmp").mkdir(exist_ok=True)
        return {"root": repo, "token": None, "versionPath": staging_path}

    def copy_base_tree(self, repo_root: Path, destination: Path) -> None:
        _copy_without_object_stores(Path(repo_root), Path(destination))

    def seal_base(self, staging: dict[str, Any]) -> dict[str, Any]:
        staging_path = Path(staging["versionPath"]).resolve()
        version = staging_path.with_name(staging_path.name[1:-len(".building")])
        if version.exists():
            if staging_path.exists():
                remove_base_version(staging_path)
            return {"image": version, "versionPath": version, "token": staging.get("token")}
        if _btrfs_path(staging_path):
            _run(["btrfs", "subvolume", "snapshot", "-r", str(staging_path), str(version)])
            _remove_btrfs_version(staging_path)
        else:
            os.replace(staging_path, version)
        return {"image": version, "versionPath": version, "token": staging.get("token")}

    def clone_workspace(self, base_image: Path, agent_dir: Path) -> Path:
        del base_image
        agent_dir = Path(agent_dir)
        agent_dir.mkdir(parents=True, exist_ok=True)
        (agent_dir / "u").mkdir(exist_ok=True)
        (agent_dir / "w").mkdir(exist_ok=True)
        return agent_dir

    def mount_workspace(self, layer: Path, mount: Path, *,
                        base_image: Path | None = None) -> dict[str, Any]:
        if base_image is None:
            raise ValueError("Linux overlay mount requires a frozen base_image")
        layer, mount, base_image = Path(layer).resolve(), Path(mount).resolve(), Path(base_image).resolve()
        upper, work = layer / "u", layer / "w"
        upper.mkdir(parents=True, exist_ok=True)
        work.mkdir(parents=True, exist_ok=True)
        mount.mkdir(parents=True, exist_ok=True)
        pid = self._ensure_namespace()
        if not _mount_exists(pid, mount):
            options = f"lowerdir={base_image},upperdir={upper},workdir={work},userxattr"
            _run(self._nsenter(pid, keep_caps=True) + [sys.executable, "-c", _OVERLAY_MOUNT,
                                                        "overlay", str(mount), options])
        return {"mount": str(mount), "layer": str(layer), "baseImage": str(base_image), "pid": pid}

    def sync_delta(self, repo_root: Path, target_repo: Path, token: object) -> object:
        del token
        repo_root, target_repo = Path(repo_root).resolve(), Path(target_repo).resolve()
        pid = self._ensure_namespace()
        args = self._nsenter(pid) + ["rsync", "-a", "--delete"]
        for store in _git_object_stores(repo_root):
            relative = store.relative_to(repo_root).as_posix()
            args.append(f"--exclude=/{relative}/***")
        args.extend([str(repo_root) + "/", str(target_repo) + "/"])
        _run(args)
        return {"synced": True}

    def unmount_workspace(self, mount: Path, *, force: bool = False) -> None:
        del force
        mount = Path(mount).resolve()
        state_path = _namespace_state_path()
        try:
            record = json.loads(state_path.read_text(encoding="utf-8"))
        except (FileNotFoundError, json.JSONDecodeError):
            return
        if not _namespace_alive(record):
            return
        pid = int(record["pid"])
        if _mount_exists(pid, mount):
            _run(self._nsenter(pid, keep_caps=True) + [sys.executable, "-c", _OVERLAY_UNMOUNT,
                                                        str(mount)])

    def remove_layer(self, agent_dir: Path) -> None:
        agent_dir = Path(agent_dir)
        if not agent_dir.exists():
            return
        work = agent_dir / "w"
        if work.exists():
            _run(["chmod", "-R", "u+rwx", str(work)])
        shutil.rmtree(agent_dir, ignore_errors=False)

    def remove_base_version(self, path: Path) -> None:
        remove_base_version(path)

    def private_bytes(self, path: Path) -> int:
        path = Path(path)
        upper = path / "u" if (path / "u").is_dir() else path
        total = 0
        for current, _dirs, files in os.walk(upper, followlinks=False):
            for name in files:
                item = Path(current) / name
                try:
                    info = item.lstat()
                except FileNotFoundError:
                    continue
                total += info.st_size
        return total

    def exec_prefix(self) -> list[str]:
        return self._nsenter(self._ensure_namespace())

    @staticmethod
    def _nsenter(pid: int, *, keep_caps: bool = False) -> list[str]:
        args = ["nsenter", "-t", str(pid), "-U", "-m", "--preserve-credentials"]
        if keep_caps:
            args.append("--keep-caps")
        return args + ["--"]

    def _ensure_namespace(self) -> int:
        state_path = _namespace_state_path()
        state_path.parent.mkdir(parents=True, exist_ok=True)
        lock_path = state_path.with_suffix(".lock")
        with lock_path.open("a+") as lock:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
            try:
                record = json.loads(state_path.read_text(encoding="utf-8"))
            except (FileNotFoundError, json.JSONDecodeError):
                record = {}
            if _namespace_alive(record):
                return int(record["pid"])
            stale_pid = record.get("pid")
            if isinstance(stale_pid, int) and stale_pid in _HOLDERS:
                _HOLDERS.pop(stale_pid).poll()
            command = ["unshare", "-U", "--map-current-user", "-m", "--propagation", "private",
                       "sleep", "infinity"]
            process = subprocess.Popen(command, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                       start_new_session=True, close_fds=True)
            deadline = time.monotonic() + 8
            start_time = None
            while time.monotonic() < deadline:
                start_time = _proc_start_time(process.pid)
                if start_time is not None and _namespace_alive(
                    {"pid": process.pid, "startTime": start_time}
                ):
                    break
                if process.poll() is not None:
                    raise RuntimeError("Could not start the Linux workspace namespace holder")
                time.sleep(0.025)
            if start_time is None:
                process.kill()
                process.wait()
                raise TimeoutError("Timed out while starting the Linux namespace holder")
            record = {"pid": process.pid, "startTime": start_time}
            _HOLDERS[process.pid] = process
            _write_json(state_path, record)
            return process.pid


def _mount_exists(pid: int, mount: Path) -> bool:
    expected = str(mount).replace(" ", r"\040").replace("\t", r"\011").replace("\n", r"\012")
    try:
        lines = Path(f"/proc/{pid}/mountinfo").read_text(encoding="utf-8").splitlines()
    except (FileNotFoundError, PermissionError):
        return False
    return any(line.split(" ")[4] == expected for line in lines if len(line.split(" ")) > 5)


def _btrfs_path(path: Path) -> bool:
    return _run(["stat", "-f", "-c", "%T", str(path)]).stdout.strip() == "btrfs"


def _remove_btrfs_version(path: Path) -> None:
    path = Path(path)
    _run(["btrfs", "property", "set", "-ts", str(path), "ro", "false"])
    _run(["chmod", "-R", "u+rwx", str(path)])
    for current, dirs, files in os.walk(path, topdown=False, followlinks=False):
        current_path = Path(current)
        for name in files:
            item = current_path / name
            if item.is_symlink() or item.is_file():
                item.unlink()
        for name in dirs:
            item = current_path / name
            if item.is_symlink():
                item.unlink()
            else:
                try:
                    item.rmdir()
                except OSError:
                    shutil.rmtree(item)
    path.rmdir()


def remove_base_version(path: Path) -> None:
    """Delete a base version without root, including a read-only btrfs snapshot."""
    path = Path(path)
    if not path.exists():
        return
    if _btrfs_path(path):
        _remove_btrfs_version(path)
    else:
        _run(["chmod", "-R", "u+rwx", str(path)])
        shutil.rmtree(path)


if __name__ == "__main__":
    print("Linux workspace backend")
