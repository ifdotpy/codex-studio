"""Apply reviewed local updates without replacing the running backend."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import threading
import time
from types import ModuleType
import uuid


class LiveUpdates:
    def __init__(self, runtime, scripts=None, interval=2):
        self.runtime = runtime
        self.scripts = Path(scripts or Path(__file__).parent).resolve()
        self.interval = interval
        self._state = {"status": "idle"}
        self._state_lock = threading.Lock()
        self._tick_lock = threading.Lock()
        self._stop = threading.Event()
        self._thread = None
        self._successful = None
        self._manager_id = uuid.uuid4().hex
        self._scope = None
        self._attempt_key = None
        self._attempts = 0
        self._retry_at = 0

    def status(self):
        with self._state_lock:
            return dict(self._state)

    def _record(self, state):
        state = {**state, "pid": os.getpid(), "managerId": self._manager_id, "scope": self._scope}
        with self._state_lock:
            self._state = state
        path = Path(self.runtime.root) / "live-update.json"
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(mode="w", dir=path.parent,
                                             prefix=".live-update-", delete=False) as stream:
                temporary = stream.name
                json.dump(state, stream, sort_keys=True)
                stream.write("\n")
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, path)
            temporary = None
            descriptor = os.open(path.parent, os.O_RDONLY)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
        except OSError as error:
            with self._state_lock:
                self._state = {**state, "receiptError": str(error)}
        finally:
            if temporary is not None:
                try:
                    os.unlink(temporary)
                except OSError:
                    pass

    def _validate(self, raw):
        manifest = json.loads(raw)
        if not isinstance(manifest, dict) or manifest.get("version") != 1:
            raise ValueError("Unsupported live update manifest version")
        identity = manifest.get("id")
        if not isinstance(identity, str) or not identity.strip() or len(identity) > 200:
            raise ValueError("Invalid live update manifest identity")
        if not isinstance(manifest.get("scope"), str) or not manifest["scope"].strip():
            raise ValueError("The live update must declare its patch scope")
        if manifest.get("python") != [3, 14] or sys.version_info[:2] != (3, 14):
            raise ValueError("The live update requires Python 3.14")
        patch = manifest.get("patch")
        if not isinstance(patch, str) or not re.fullmatch(r"codex_[a-z0-9_]+_update\.py", patch):
            raise ValueError("Invalid live update patch name")
        inputs = manifest.get("inputs")
        if not isinstance(inputs, dict) or not inputs:
            raise ValueError("Missing live update source hashes")
        names = {path.name for path in self.scripts.iterdir()
                 if path.is_file() and (path.suffix == ".py" or path.name == "codex-canvas")}
        if "codex-canvas" not in names or patch not in names or set(inputs) != names:
            raise ValueError("Live update inputs do not cover the complete source tree")
        contents = {}
        for name, expected in inputs.items():
            if (not isinstance(name, str) or Path(name).name != name
                    or not isinstance(expected, str) or not re.fullmatch(r"[a-f0-9]{64}", expected)):
                raise ValueError("Invalid live update source hash")
            path = self.scripts / name
            if path.is_symlink() or path.resolve().parent != self.scripts:
                raise ValueError("Live update source must be a local regular file")
            content = path.read_bytes()
            if hashlib.sha256(content).hexdigest() != expected:
                raise ValueError("Live update source hash differs: " + name)
            contents[name] = content
        return manifest, contents[patch]

    def _sources(self):
        result = []
        for path in sorted(self.scripts.iterdir()):
            if path.suffix == ".py" or path.name == "codex-canvas":
                info = path.lstat()
                result.append((path.name, info.st_ino, info.st_mode, info.st_size,
                               info.st_mtime_ns, info.st_ctime_ns))
        return tuple(result)

    def tick(self):
        if self._stop.is_set() or self.runtime.closed or not self._tick_lock.acquire(blocking=False):
            return
        key = None
        identity = None
        try:
            path = self.scripts / "studio-live-update.json"
            try:
                raw = path.read_bytes()
            except FileNotFoundError:
                self._successful = None
                return
            key = hashlib.sha256(raw).hexdigest()
            source_signature = self._sources()
            attempt_key = (key, source_signature)
            if attempt_key != self._attempt_key:
                self._successful = None
                self._scope = None
                self._attempt_key, self._attempts, self._retry_at = attempt_key, 0, 0
            if key == self._successful:
                state = self.status()
                if state.pop("receiptError", None) is not None:
                    self._record(state)
                return
            if time.monotonic() < self._retry_at:
                return
            self._attempts += 1
            # Publishers hold this same lock exclusively while replacing sources
            # and publish the manifest last. Never wait behind a publisher here.
            with (self.scripts / ".studio-update.lock").open("a+b") as lock:
                fcntl.flock(lock, fcntl.LOCK_SH | fcntl.LOCK_NB)
                if path.read_bytes() != raw:
                    raise ValueError("Live update publication changed; retry required")
                manifest, source = self._validate(raw)
                identity = manifest["id"]
                self._scope = manifest.get("scope")
                if self._stop.is_set() or self.runtime.closed:
                    return
                self._record({"status": "applying", "manifestId": identity,
                              "manifestHash": key, "attempt": self._attempts,
                              "checkedAt": time.time()})
                name = manifest["patch"][:-3]
                module = ModuleType(name)
                module.__file__ = str(self.scripts / manifest["patch"])
                module.__package__ = ""
                exec(compile(source, module.__file__, "exec", dont_inherit=True), vars(module))
                apply = getattr(module, "apply", None)
                if not callable(apply):
                    raise ValueError("The live update has no apply function")
                result = apply(self.runtime)
                if not isinstance(result, dict) or result.get("status") not in {"applied", "already_applied"}:
                    raise ValueError("The live update did not confirm application")
                self._successful = key
                self._record({"status": "applied", "manifestId": identity,
                              "manifestHash": key, "patchStatus": result["status"],
                              "attempt": self._attempts, "appliedAt": time.time()})
        except Exception as error:
            self._retry_at = time.monotonic() + min(30, 2 ** min(self._attempts, 5))
            self._record({"status": "waiting" if isinstance(error, BlockingIOError) else "failed",
                          "manifestId": identity, "manifestHash": key,
                          "attempt": self._attempts, "error": str(error),
                          "checkedAt": time.time()})
        finally:
            self._tick_lock.release()

    def start(self):
        with self._state_lock:
            if self._thread is not None:
                return self
            self._thread = threading.Thread(target=self._run, name="studio-live-updates", daemon=True)
            self._thread.start()
        return self

    def _run(self):
        while not self._stop.is_set() and not self.runtime.closed:
            self.tick()
            self._stop.wait(self.interval)

    def close(self):
        self._stop.set()
        thread = self._thread
        if thread is not None and thread is not threading.current_thread():
            thread.join(timeout=12)


_START_LOCK = threading.Lock()


def start(runtime):
    with _START_LOCK:
        manager = getattr(runtime, "live_updates", None)
        if manager is None:
            manager = runtime.live_updates = LiveUpdates(runtime)
        return manager.start()
