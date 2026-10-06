"""ASIF image backend for macOS workspaces."""

from __future__ import annotations

import ctypes
import os
import plistlib
import re
import shutil
import signal
import stat
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ATTR_CMNEXT_PRIVATESIZE = 0x00000008
FSOPT_NOFOLLOW = 0x00000001
FSOPT_ATTR_CMN_EXTENDED = 0x00000020
_FSEVENT_LOCK = threading.Lock()
_FSEVENT_API = None


class _FSEventsHistoryLost(RuntimeError):
    pass


class _AttrList(ctypes.Structure):
    _fields_ = [('bitmapcount', ctypes.c_uint16), ('reserved', ctypes.c_uint16),
                ('commonattr', ctypes.c_uint32), ('volattr', ctypes.c_uint32),
                ('dirattr', ctypes.c_uint32), ('fileattr', ctypes.c_uint32),
                ('forkattr', ctypes.c_uint32)]


def _run(args, *, timeout=120, check=True, **kwargs):
    return subprocess.run([str(x) for x in args], timeout=timeout, check=check,
                          capture_output=True, **kwargs)


def _diskutil_plist(args):
    result = _run(['diskutil', *args, '--plist'], timeout=300)
    return plistlib.loads(result.stdout)


def _mounted(path):
    return Path(path).is_mount()


def _device_for_mount(mount):
    mount = Path(mount).resolve()
    info = plistlib.loads(_run(['hdiutil', 'info', '-plist'], timeout=30).stdout)
    for image in info.get('images', []):
        entities = image.get('system-entities', [])
        matching = next((entry for entry in entities
                         if entry.get('mount-point') and Path(entry['mount-point']).resolve() == mount), None)
        if matching:
            entry = next((item for item in entities
                          if re.fullmatch(r'/dev/disk\d+', item.get('dev-entry', ''))), None)
            if entry:
                return entry['dev-entry']
            return re.sub(r's\d+$', '', matching.get('dev-entry', ''))
    return None


def _copy_tree_parallel(source, dest, excludes=()):
    source, dest = Path(source).resolve(), Path(dest).resolve()
    exclusions = [Path(value).as_posix() for value in excludes]
    top = sorted(source.iterdir(), key=lambda p: p.name)
    shards = [[] for _ in range(min(16, max(1, len(top))))]
    for index, entry in enumerate(top):
        shards[index % len(shards)].append(entry.name)

    def copy_shard(names):
        if not names:
            return
        # BSD tar accepts path operands and excludes relative to its cwd.
        command = ['tar', '-C', str(source), '-cf', '-', '--no-mac-metadata']
        for relative in exclusions:
            for name in names:
                if relative == name or relative.startswith(name + '/'):
                    command.extend(['--exclude', relative, '--exclude', relative + '/*'])
        command.extend(['--', *names])
        producer = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        consumer = subprocess.run(['tar', '-C', str(dest), '-xf', '-'], stdin=producer.stdout,
                                  capture_output=True, timeout=1800)
        producer.stdout.close()
        stderr = producer.communicate(timeout=1800)[1]
        if producer.returncode or consumer.returncode:
            raise RuntimeError((stderr + consumer.stderr).decode(errors='replace')[-3000:])

    with ThreadPoolExecutor(max_workers=len(shards), thread_name_prefix='studio-base-tar') as pool:
        futures = [pool.submit(copy_shard, names) for names in shards if names]
        for future in futures:
            future.result()


def _fsevent_api():
    global _FSEVENT_API
    if _FSEVENT_API is not None:
        return _FSEVENT_API
    with _FSEVENT_LOCK:
        if _FSEVENT_API is None:
            core = ctypes.CDLL('/System/Library/Frameworks/CoreServices.framework/CoreServices')
            foundation = ctypes.CDLL('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
            dispatch = ctypes.CDLL('/usr/lib/system/libdispatch.dylib')
            core.FSEventsGetCurrentEventId.restype = ctypes.c_uint64
            core.FSEventStreamCreate.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p,
                                                ctypes.c_void_p, ctypes.c_uint64, ctypes.c_double,
                                                ctypes.c_uint32]
            core.FSEventStreamCreate.restype = ctypes.c_void_p
            core.FSEventStreamSetDispatchQueue.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
            core.FSEventStreamStart.argtypes = [ctypes.c_void_p]
            core.FSEventStreamFlushSync.argtypes = [ctypes.c_void_p]
            core.FSEventStreamStop.argtypes = [ctypes.c_void_p]
            core.FSEventStreamInvalidate.argtypes = [ctypes.c_void_p]
            core.FSEventStreamRelease.argtypes = [ctypes.c_void_p]
            foundation.CFArrayCreate.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p),
                                                 ctypes.c_long, ctypes.c_void_p]
            foundation.CFArrayCreate.restype = ctypes.c_void_p
            foundation.CFArrayGetCount.argtypes = [ctypes.c_void_p]
            foundation.CFArrayGetCount.restype = ctypes.c_long
            foundation.CFArrayGetValueAtIndex.argtypes = [ctypes.c_void_p, ctypes.c_long]
            foundation.CFArrayGetValueAtIndex.restype = ctypes.c_void_p
            foundation.CFStringCreateWithFileSystemRepresentation.argtypes = [ctypes.c_void_p,
                                                                               ctypes.c_char_p]
            foundation.CFStringCreateWithFileSystemRepresentation.restype = ctypes.c_void_p
            dispatch.dispatch_get_global_queue.argtypes = [ctypes.c_long, ctypes.c_ulong]
            dispatch.dispatch_get_global_queue.restype = ctypes.c_void_p
            _FSEVENT_API = core, foundation, dispatch
    return _FSEVENT_API


def _read_events(root, since):
    """Return (paths, flags, newest id); event history loss is explicit."""
    core, foundation, dispatch = _fsevent_api()
    current = int(core.FSEventsGetCurrentEventId())
    if int(since) >= current:
        return [], current
    encoded = os.fsencode(root)
    cfpath = ctypes.c_void_p()
    cfpath = foundation.CFStringCreateWithFileSystemRepresentation(None, encoded)
    paths = (ctypes.c_void_p * 1)(cfpath)
    array = foundation.CFArrayCreate(None, paths, 1, None)
    collected = []
    done = threading.Event()
    error = []
    callback_type = ctypes.CFUNCTYPE(None, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_size_t,
                                     ctypes.POINTER(ctypes.c_char_p), ctypes.POINTER(ctypes.c_uint32),
                                     ctypes.POINTER(ctypes.c_uint64))

    def callback(_stream, _info, count, cfpaths, flags_ptr, ids_ptr):
        try:
            flags = ctypes.cast(flags_ptr, ctypes.POINTER(ctypes.c_uint32))
            ids = ctypes.cast(ids_ptr, ctypes.POINTER(ctypes.c_uint64))
            for i in range(count):
                collected.append((os.fsdecode(cfpaths[i]), int(flags[i]), int(ids[i])))
                if int(flags[i]) & 0x10:
                    done.set()
        except BaseException as exc:
            error.append(exc)

    callback_ref = callback_type(callback)
    # FSEventStreamContext is five pointer-sized fields.
    class _Context(ctypes.Structure):
        _fields_ = [('version', ctypes.c_long), ('info', ctypes.c_void_p),
                    ('retain', ctypes.c_void_p), ('release', ctypes.c_void_p),
                    ('copyDescription', ctypes.c_void_p)]
    ctx = _Context(0, None, None, None, None)
    create = core.FSEventStreamCreate
    create.argtypes = [ctypes.c_void_p, callback_type, ctypes.POINTER(_Context), ctypes.c_void_p,
                       ctypes.c_uint64, ctypes.c_double, ctypes.c_uint32]
    stream = create(None, callback_ref, ctypes.byref(ctx), array, int(since), 0.1, 0x10 | 0x2)
    if not stream:
        raise OSError('FSEventStreamCreate failed')
    try:
        core.FSEventStreamSetDispatchQueue(stream, dispatch.dispatch_get_global_queue(0, 0))
        if not core.FSEventStreamStart(stream):
            raise OSError('FSEventStreamStart failed')
        if not done.wait(120):
            raise TimeoutError('FSEvents did not complete event history')
        if error:
            raise error[0]
    finally:
        core.FSEventStreamStop(stream)
        core.FSEventStreamInvalidate(stream)
        core.FSEventStreamRelease(stream)
    latest = max([int(since), *(item[2] for item in collected)])
    if any(flags & (0x2 | 0x4 | 0x8) for _path, flags, _event_id in collected):
        raise _FSEventsHistoryLost('FSEvents dropped or wrapped event history')
    return collected, latest


def current_event_id(_root):
    core, _foundation, _dispatch = _fsevent_api()
    return int(core.FSEventsGetCurrentEventId())


def _rsync_folder(source, target, excludes=(), *, destination_root=None):
    source, target = Path(source), Path(target)
    destination_root = Path(destination_root) if destination_root is not None else target
    _prepare_delta_directory(target, destination_root)
    args = ['rsync', '-a', '--delete']
    args.extend('--exclude=/' + Path(value).as_posix().rstrip('/') + '/***' for value in excludes)
    result = _run([*args, str(source) + '/', str(target) + '/'], timeout=1800, check=False)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors='replace')[-3000:])


def _prepare_delta_parent(target, destination_root):
    target, destination_root = Path(target), Path(destination_root)
    parent = target.parent
    relative = parent.relative_to(destination_root)
    current = destination_root
    for part in relative.parts:
        current = current / part
        try:
            info = current.lstat()
        except FileNotFoundError:
            current.mkdir()
            continue
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
            if stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode):
                shutil.rmtree(current)
            else:
                current.unlink()
            current.mkdir()


def _prepare_delta_directory(target, destination_root):
    target, destination_root = Path(target), Path(destination_root)
    if target != destination_root:
        _prepare_delta_parent(target, destination_root)
    try:
        info = target.lstat()
    except FileNotFoundError:
        target.mkdir(parents=target == destination_root)
        return
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
        if stat.S_ISDIR(info.st_mode) and not stat.S_ISLNK(info.st_mode):
            shutil.rmtree(target)
        else:
            target.unlink()
        target.mkdir()


def _copy_delta_entry(source, target, destination_root=None):
    if not source.exists() and not source.is_symlink():
        if destination_root is not None:
            _prepare_delta_parent(target, destination_root)
        if target.is_dir() and not target.is_symlink():
            shutil.rmtree(target)
        else:
            target.unlink(missing_ok=True)
        return
    if source.is_dir() and not source.is_symlink():
        if destination_root is not None:
            _prepare_delta_parent(target, destination_root)
        if target.is_symlink() or (target.exists() and not target.is_dir()):
            target.unlink()
        target.mkdir(parents=True, exist_ok=True)
        return
    if destination_root is None:
        target.parent.mkdir(parents=True, exist_ok=True)
    else:
        _prepare_delta_parent(target, destination_root)
    if source.is_symlink():
        if target.is_dir() and not target.is_symlink():
            shutil.rmtree(target)
        else:
            target.unlink(missing_ok=True)
        target.symlink_to(os.readlink(source))
        return
    try:
        target_info = target.lstat()
    except FileNotFoundError:
        target_info = None
    if target_info is not None:
        if target.is_dir() and not target.is_symlink():
            shutil.rmtree(target)
        else:
            target.unlink()
    try:
        os.clonefile(source, target)
    except (AttributeError, OSError):
        shutil.copy2(source, target)


class Backend:
    def supported(self, repo_root):
        if sys.platform != 'darwin':
            return False, 'macOS image workspaces require Darwin'
        if not Path(repo_root).is_dir():
            return False, 'Repository folder does not exist'
        if shutil.which('diskutil') is None:
            return False, 'diskutil is unavailable'
        return True, ''

    def exclude_store(self, store):
        store = Path(store)
        store.mkdir(parents=True, exist_ok=True)
        marker = store / '.time-machine-excluded'
        if not marker.exists():
            _run(['tmutil', 'addexclusion', str(store)], timeout=30)
            marker.touch()

    def current_event_id(self, repo_root):
        return current_event_id(repo_root)

    def base_needs_refresh(self, repo_root, base_state):
        if not base_state or not base_state.get('token'):
            return False
        try:
            events, _newest = _read_events(repo_root, int(base_state['token']))
        except (OSError, RuntimeError, TimeoutError):
            return True
        if len(events) >= 1000:
            return True
        changed_bytes = 0
        for value, _flags, _event_id in events:
            try:
                path = Path(value)
                if not path.is_absolute():
                    path = Path(repo_root) / path
                if path.is_file():
                    changed_bytes += path.stat().st_size
            except OSError:
                continue
        base_bytes = self.private_bytes(Path(base_state.get('image', ''))) or 0
        return changed_bytes >= max(16 * 1024**2, int(base_bytes * .35))

    def open_base_refresh(self, repo_root, repo_key, version, old_state):
        store = Path(os.environ.get('CODEX_WORKSPACE_STORE') or
                     Path.home() / '.local/state/codex-agents/workspaces')
        version_path = store / 'bases' / repo_key / 'versions' / version
        image = version_path / 'base.asif'
        mount = version_path / 'mount'
        version_path.mkdir(parents=True, exist_ok=True)
        mount.mkdir(parents=True, exist_ok=True)
        result = _run(['cp', '-c', str(old_state['image']), str(image)], timeout=300, check=False)
        if result.returncode:
            shutil.copy2(old_state['image'], image)
        _diskutil_plist(['image', 'attach', '--nobrowse', '--mountPoint', str(mount), str(image)])
        (mount / '.metadata_never_index').touch()
        (mount / 'home').mkdir(exist_ok=True)
        (mount / 'tmp').mkdir(exist_ok=True)
        return {'root': mount / 'repo', 'image': image, 'mount': mount,
                'versionPath': version_path, 'token': old_state.get('token'), 'version': version,
                'refresh': True}

    def open_base_staging(self, repo_root, repo_key, version):
        root = Path(os.environ.get('CODEX_WORKSPACE_STORE') or
                    Path.home() / '.local/state/codex-agents/workspaces')
        version_path = root / 'bases' / repo_key / 'versions' / version
        image = version_path / 'base.asif'
        mount = version_path / 'mount'
        version_path.mkdir(parents=True, exist_ok=True)
        mount.mkdir(parents=True, exist_ok=True)
        if _mounted(mount):
            self.unmount_workspace(mount, force=False)
        if image.exists():
            image.unlink()
        _run(['diskutil', 'image', 'create', 'blank', '--format', 'ASIF', '--size', '200g',
              '--volumeName', 'StudioBase', '--fs', 'APFS', str(image)], timeout=900)
        token = current_event_id(repo_root)
        _diskutil_plist(['image', 'attach', '--nobrowse', '--mountPoint', str(mount), str(image)])
        (mount / '.metadata_never_index').touch()
        (mount / 'repo').mkdir(exist_ok=True)
        (mount / 'home').mkdir(exist_ok=True)
        (mount / 'tmp').mkdir(exist_ok=True)
        return {'root': mount / 'repo', 'image': image, 'mount': mount,
                'versionPath': version_path, 'token': token, 'version': version}

    def copy_base_tree(self, repo_root, destination, *, excludes=()):
        _copy_tree_parallel(Path(repo_root).resolve(), Path(destination), excludes)

    def seal_base(self, staging):
        mount = Path(staging['mount'])
        self.unmount_workspace(mount)
        return {'image': Path(staging['image']), 'versionPath': Path(staging['versionPath']),
                'token': staging.get('token')}

    def clone_workspace(self, base_image, agent_dir):
        base_image, agent_dir = Path(base_image), Path(agent_dir)
        agent_dir.mkdir(parents=True, exist_ok=True)
        image = agent_dir / 'workspace.asif'
        if not image.exists():
            result = _run(['cp', '-c', str(base_image), str(image)], timeout=300, check=False)
            if result.returncode:
                # The safe fallback is a real copy; never attach the shared base writable.
                shutil.copy2(base_image, image)
        return image

    def mount_workspace(self, layer, mount, *, base_image=None):
        layer, mount = Path(layer), Path(mount)
        mount.mkdir(parents=True, exist_ok=True)
        if _mounted(mount):
            return {'mount': str(mount)}
        _diskutil_plist(['image', 'attach', '--nobrowse', '--mountPoint', str(mount), str(layer)])
        return {'mount': str(mount)}

    def sync_delta(self, repo_root, target_repo, token, *, excludes=()):
        current = token.get('token') if isinstance(token, dict) else token
        excluded = {Path(value) for value in excludes}
        root = Path(repo_root).resolve()
        target = Path(target_repo)
        changed_paths = set()
        scan_paths = set()

        def apply(events):
            did_copy = False
            scans = []
            paths = set()
            for value, flags, _event_id in events:
                path = Path(value)
                if path.is_absolute():
                    try:
                        path = path.relative_to(root)
                    except ValueError:
                        continue
                if flags & 0x1:
                    scans.append(path)
                    scan_paths.add(path.as_posix() or '.')
                    changed_paths.add(path.as_posix() or '.')
                else:
                    paths.add(path)
            for folder in scans:
                if any(folder == item or item in folder.parents for item in excluded):
                    continue
                nested_excludes = []
                for item in excluded:
                    try:
                        nested_excludes.append(item.relative_to(folder))
                    except ValueError:
                        continue
                source_folder, target_folder = root / folder, target / folder
                if source_folder.exists() and source_folder.is_dir() and not source_folder.is_symlink():
                    _rsync_folder(source_folder, target_folder, nested_excludes,
                                  destination_root=target)
                else:
                    _copy_delta_entry(source_folder, target_folder, target)
                did_copy = True
            for rel in sorted(paths, key=lambda path: (len(path.parts), str(path))):
                if any(rel == item or item in rel.parents for item in excluded):
                    continue
                if rel == Path('.'):
                    continue
                source, dest = root / rel, target / rel
                changed_paths.add(rel.as_posix() or '.')
                _copy_delta_entry(source, dest, target)
                did_copy = True
            return did_copy

        for _pass in range(12):
            try:
                events, newest = _read_events(repo_root, int(current or 0))
            except _FSEventsHistoryLost:
                _rsync_folder(root, target, excludes)
                return {'token': current_event_id(root), 'changedPaths': ['.'],
                        'scanPaths': ['.'], 'historyLost': True, 'refreshBase': True}
            if not events:
                return {'token': newest, 'changedPaths': sorted(changed_paths),
                        'scanPaths': sorted(scan_paths), 'historyLost': False}
            # A MustScanSubDirs event on the watched root is the only non-loss
            # case that needs a full-tree sync. A nested event scans that folder.
            root_scan = any((flags & 0x1) and Path(value).resolve() == root
                            for value, flags, _event_id in events)
            if root_scan:
                _rsync_folder(root, target, excludes)
                return {'token': newest, 'changedPaths': sorted(changed_paths | {'.'}),
                        'scanPaths': sorted(scan_paths | {'.'}), 'historyLost': False,
                        'refreshBase': True}
            did_copy = apply(events)
            current = newest
            if not did_copy:
                return {'token': current, 'changedPaths': sorted(changed_paths),
                        'scanPaths': sorted(scan_paths), 'historyLost': False}

        # One final delta pass bounds work during sustained writes.
        try:
            events, newest = _read_events(repo_root, int(current or 0))
        except _FSEventsHistoryLost:
            _rsync_folder(root, target, excludes)
            return {'token': current_event_id(root), 'changedPaths': ['.'],
                    'scanPaths': ['.'], 'historyLost': True, 'refreshBase': True}
        if events:
            root_scan = any((flags & 0x1) and Path(value).resolve() == root
                            for value, flags, _event_id in events)
            if root_scan:
                _rsync_folder(root, target, excludes)
                scan_paths.add('.')
                changed_paths.add('.')
            else:
                apply(events)
            current = newest
        return {'token': current, 'changedPaths': sorted(changed_paths),
                'scanPaths': sorted(scan_paths), 'historyLost': False}

    def unmount_workspace(self, mount, *, force=False):
        mount = Path(mount)
        if not _mounted(mount):
            return
        if force:
            result = _run(['lsof', '-t', '+f', '--', str(mount)], check=False)
            pids = {int(value) for value in result.stdout.decode().split() if value.isdigit()}
            for pid in pids:
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and any(_pid_exists(pid) for pid in pids):
                time.sleep(.1)
            for pid in pids:
                if _pid_exists(pid):
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
        device = _device_for_mount(mount)
        if not device:
            raise RuntimeError(f'Cannot find the disk image device for mounted workspace: {mount}')
        result = _run(['diskutil', 'eject', device], timeout=120, check=False)
        if result.returncode and _mounted(mount):
            raise RuntimeError(result.stderr.decode(errors='replace')[-3000:])

    def remove_layer(self, agent_dir):
        shutil.rmtree(agent_dir, ignore_errors=False)

    def remove_base_version(self, path):
        path = Path(path)
        mount = path / 'mount' if path.is_dir() else path.parent / 'mount'
        if mount.exists() and _mounted(mount):
            self.unmount_workspace(mount, force=False)
        image = path / 'base.asif' if path.is_dir() else path
        if image.exists():
            image.unlink()
        if path.is_dir():
            shutil.rmtree(path, ignore_errors=True)

    def private_bytes(self, path):
        return _apfs_private_bytes(Path(path))

    def exec_prefix(self):
        return []


def _pid_exists(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _apfs_private_bytes(path):
    try:
        function = ctypes.CDLL(None, use_errno=True).getattrlist
        function.argtypes = [ctypes.c_char_p, ctypes.POINTER(_AttrList), ctypes.c_void_p,
                             ctypes.c_size_t, ctypes.c_ulong]
        function.restype = ctypes.c_int
        attrs = _AttrList(5, 0, 0, 0, 0, 0, ATTR_CMNEXT_PRIVATESIZE)
        output = ctypes.create_string_buffer(16)
        options = FSOPT_ATTR_CMN_EXTENDED | FSOPT_NOFOLLOW
        if function(os.fsencode(path), ctypes.byref(attrs), output, len(output), options) != 0:
            return None
        length = int.from_bytes(output.raw[:4], sys.byteorder)
        if length < 12 or length > len(output):
            return None
        return int.from_bytes(output.raw[4:12], sys.byteorder)
    except (AttributeError, OSError, TypeError, ValueError):
        return None
