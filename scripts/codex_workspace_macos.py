"""ASIF image backend for macOS workspaces."""

from __future__ import annotations

import ctypes
import os
import plistlib
import re
import shutil
import signal
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


def _repo_markers(root):
    """Find .git roots, including submodules, without walking object stores."""
    root = Path(root).resolve()
    found = [root]
    for current, dirs, files in os.walk(root, followlinks=False):
        here = Path(current)
        if '.git' in dirs or '.git' in files:
            candidate = here
            if candidate != root:
                found.append(candidate)
        dirs[:] = [name for name in dirs if name != '.git' and not (here / name).is_symlink()]
    return list(dict.fromkeys(found))


def _object_dirs(root):
    results = []
    for candidate in _repo_markers(root):
        probe = subprocess.run(['git', '-C', str(candidate), 'rev-parse', '--show-toplevel'],
                               capture_output=True, text=True, timeout=20)
        if probe.returncode:
            continue
        top = Path(probe.stdout.strip()).resolve()
        if top != candidate.resolve():
            continue
        object_dir = subprocess.check_output(
            ['git', '-C', str(top), 'rev-parse', '--path-format=absolute', '--git-path', 'objects'],
            text=True, timeout=20).strip()
        results.append((top, Path(object_dir).resolve()))
    return results


def _is_object_store(path):
    path = Path(path)
    return path.name == 'objects' and (path.parent / 'HEAD').exists()


def _copy_tar(source, dest, excludes):
    """Copy one tree shard through system tar, omitting repository objects."""
    source, dest = Path(source), Path(dest)
    dest.mkdir(parents=True, exist_ok=True)
    command = ['tar', '-C', str(source), '-cf', '-', '--no-mac-metadata']
    for relative in excludes:
        command.extend(['--exclude', relative])
        command.extend(['--exclude', relative.rstrip('/') + '/*'])
    command.append('.')
    producer = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    consumer = subprocess.run(['tar', '-C', str(dest), '-xf', '-'], stdin=producer.stdout,
                              capture_output=True, timeout=900)
    producer.stdout.close()
    stderr = producer.communicate(timeout=900)[1]
    if producer.returncode or consumer.returncode:
        raise RuntimeError((stderr + consumer.stderr).decode(errors='replace')[-3000:])


def _copy_tree_parallel(source, dest, excludes=()):
    source, dest = Path(source).resolve(), Path(dest).resolve()
    exclusions = [Path(value).as_posix() for value in excludes]
    for root, objects in _object_dirs(source):
        try:
            relative = objects.relative_to(source)
        except ValueError:
            continue
        if _is_object_store(objects):
            exclusions.append(str(relative))
    top = sorted(source.iterdir(), key=lambda p: p.name)
    shards = [[] for _ in range(min(16, max(1, len(top))))]
    for index, entry in enumerate(top):
        shards[index % len(shards)].append(entry.name)

    def copy_shard(names):
        if not names:
            return
        shard = source.parent / ('.tar-shard-' + str(threading.get_ident()))
        shard.mkdir(parents=True, exist_ok=True)
        try:
            # BSD tar accepts path operands and excludes relative to its cwd.
            command = ['tar', '-C', str(source), '-cf', '-', '--no-mac-metadata']
            for relative in exclusions:
                for name in names:
                    if relative == name or relative.startswith(name + '/'):
                        command.extend(['--exclude', relative, '--exclude', relative + '/*'])
            command.extend(names)
            producer = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            consumer = subprocess.run(['tar', '-C', str(dest), '-xf', '-'], stdin=producer.stdout,
                                      capture_output=True, timeout=1800)
            producer.stdout.close()
            stderr = producer.communicate(timeout=1800)[1]
            if producer.returncode or consumer.returncode:
                raise RuntimeError((stderr + consumer.stderr).decode(errors='replace')[-3000:])
        finally:
            shard.rmdir()

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
        if not done.wait(3):
            raise TimeoutError('FSEvents did not complete event history')
        if error:
            raise error[0]
    finally:
        core.FSEventStreamStop(stream)
        core.FSEventStreamInvalidate(stream)
        core.FSEventStreamRelease(stream)
    latest = max([int(since), *(item[2] for item in collected)])
    return collected, latest


def current_event_id(_root):
    core, _foundation, _dispatch = _fsevent_api()
    return int(core.FSEventsGetCurrentEventId())


def _rsync_folder(source, target, excludes=()):
    source, target = Path(source), Path(target)
    target.mkdir(parents=True, exist_ok=True)
    args = ['rsync', '-a', '--delete', '--exclude=**/.git/objects/***']
    args.extend('--exclude=/' + Path(value).as_posix().rstrip('/') + '/***' for value in excludes)
    result = _run([*args, str(source) + '/', str(target) + '/'], timeout=1800, check=False)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors='replace')[-3000:])


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
        if len(events) >= 5000:
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
            self.unmount_workspace(mount, force=True)
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
        current = token
        excluded = {Path(value) for value in excludes}
        for _pass in range(12):
            try:
                events, newest = _read_events(repo_root, int(current or 0))
            except (OSError, RuntimeError, TimeoutError):
                root, target = Path(repo_root), Path(target_repo)
                _rsync_folder(root, target, excludes)
                return current_event_id(root)
            if not events:
                return newest
            root = Path(repo_root).resolve()
            target = Path(target_repo)
            full_scan = []
            paths = set()
            for value, flags, _event_id in events:
                # MustScanSubDirs is 0x1. RootChanged/UserDropped/KernelDropped also force scan.
                if flags & (0x1 | 0x20 | 0x40 | 0x80):
                    full_scan.append(Path(value))
                    continue
                path = Path(value)
                if path.is_absolute():
                    try:
                        path = path.relative_to(root)
                    except ValueError:
                        continue
                paths.add(path)
            for folder in full_scan:
                if folder.is_absolute():
                    try:
                        folder = folder.relative_to(root)
                    except ValueError:
                        folder = Path('.')
                nested_excludes = []
                for item in excluded:
                    try:
                        nested_excludes.append(item.relative_to(folder))
                    except ValueError:
                        continue
                _rsync_folder(root / folder, target / folder, nested_excludes)
            for rel in sorted(paths, key=lambda p: (len(p.parts), str(p))):
                if any(rel == item or item in rel.parents for item in excluded):
                    continue
                source = root / rel
                dest = target / rel
                if any(part == 'objects' and (root.joinpath(*rel.parts[:idx]) / 'HEAD').exists()
                       for idx, part in enumerate(rel.parts)):
                    continue
                try:
                    info = source.lstat()
                except FileNotFoundError:
                    if dest.is_dir() and not dest.is_symlink():
                        shutil.rmtree(dest)
                    else:
                        dest.unlink(missing_ok=True)
                    continue
                if source.is_dir() and not source.is_symlink():
                    dest.mkdir(parents=True, exist_ok=True)
                else:
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    if source.is_symlink():
                        dest.unlink(missing_ok=True)
                        dest.symlink_to(os.readlink(source))
                    else:
                        try:
                            os.clonefile(source, dest)
                        except (AttributeError, OSError):
                            shutil.copy2(source, dest)
            current = newest
        # A sustained writer or lost history must not silently report a partial sync.
        _rsync_folder(Path(repo_root), Path(target_repo), excludes)
        return current

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
            self.unmount_workspace(mount, force=True)
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
