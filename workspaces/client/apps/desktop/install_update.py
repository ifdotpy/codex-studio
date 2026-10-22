#!/usr/bin/env python3
"""Install the app build that the backend staged from a paired server (docs/peer-updates.md).

The recovery LaunchAgent runs this file from the installed app when pending-update.json exists.
Exit status: 0 installed, 75 waiting (try again later), 1 failed (the pending file is removed).
A failure before the swap leaves the installed app unchanged; a failed swap restores it.
"""
import argparse
from contextlib import contextmanager
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import signal
import sys
import time

HERE = Path(__file__).resolve().parent
INSTALLED, FAILED, WAITING = 0, 1, 75
OSASCRIPT = "/usr/bin/osascript"
OPEN = "/usr/bin/open"
# Seconds to wait for the desktop window to quit, and for the old backend to exit.
QUIT_SECONDS = 60
BACKEND_EXIT_SECONDS = 60
# The window writes this intent (pid, startedAt, executable) while it is open.
DESKTOP_INTENT = "desktop-recovery.json"
# Owner: CLOSE_SIGNAL in workspaces/client/apps/native/src-tauri/src/desktop_recovery.rs. A window
# that records it as `closeSignal` closes on SIGTERM through its save check.
CLOSE_SIGNAL = "SIGTERM"
NATIVE_EXECUTABLE = "Contents/MacOS/studio-native-client"


def load_recovery():
    spec = importlib.util.spec_from_file_location("studio_recover_backend", HERE / "recover_backend.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


recovery = load_recovery()


def load_updates(resources):
    """Use helpers shipped beside this installer, independent of the backend."""
    import desktop_updates
    import desktop_vm
    return desktop_updates, desktop_vm


def event(**fields):
    print(json.dumps({"event": "peer-update-install", **fields}), flush=True)


@contextmanager
def held(path, create=True):
    """Hold an exclusive lock, or yield False when another process holds it. Without its folder
    (create=False) no process can hold it."""
    if not create and not path.parent.is_dir():
        yield True
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            yield False
            return
        try:
            yield True
        finally:
            fcntl.flock(lease, fcntl.LOCK_UN)


def desktop_running(updates, run):
    script = f'application id "{updates.BUNDLE_ID}" is running'
    result = run([OSASCRIPT, "-e", script], 15)
    if result.returncode != 0:
        raise RuntimeError("Cannot check the desktop window: " + (result.stderr or "").strip()[-200:])
    return result.stdout.strip() == "true"


def signal_target(state, installed, started=recovery.process_start_time):
    """The PID of the open window of the installed app when it closes on SIGTERM, else None.

    The window records its PID, start time, executable and close signal in its intent. AppKit
    can refuse an Apple Event quit without user input (osascript: "User canceled. (-128)"), so
    a window that supports the signal gets it. An older window records no signal and gets the
    Apple Event quit. The PID counts only while the same process (start time) runs the app."""
    try:
        intent = json.loads((state / DESKTOP_INTENT).read_text())
    except (OSError, ValueError):
        return None
    if (not isinstance(intent, dict) or intent.get("version") != 1 or intent.get("desiredOpen") is not True
            or intent.get("closeSignal") != CLOSE_SIGNAL
            or intent.get("executable") != str(installed / NATIVE_EXECUTABLE)):
        return None
    pid = intent.get("pid")
    if type(pid) is not int or pid < 2 or not intent.get("startedAt"):
        return None
    try:
        return pid if started(pid) == intent["startedAt"] else None
    except RuntimeError:
        return None


def quit_desktop(updates, run, sleep, target=None, kill=os.kill):
    if target is not None:
        try:
            kill(target, signal.SIGTERM)
        except ProcessLookupError:
            pass  # It closed after the identity check; the wait below confirms that.
    else:
        result = run([OSASCRIPT, "-e", f'tell application id "{updates.BUNDLE_ID}" to quit'], QUIT_SECONDS)
        if result.returncode != 0:
            raise RuntimeError("The desktop window did not quit: " + (result.stderr or "").strip()[-200:])
    deadline = time.monotonic() + QUIT_SECONDS
    while desktop_running(updates, run):
        if time.monotonic() >= deadline:
            raise RuntimeError("The desktop window did not quit.")
        sleep(1)


def verify_backend(config, state):
    """Before the swap: prove which process is the backend of this state directory.

    The proof reads the installed app that started the backend (its packaged process reader,
    script path and, for Rust, executable hash) with modules of one build. After the swap the
    app holds the new build: a module imported then can need names that the modules this run
    already imported do not have, and the Rust hash is the new executable's.
    Returns the PID and its start time, or None when no backend runs."""
    data = recovery.identity(config["port"], state)
    if not data:
        return None
    recovery.verify_backend_process(data, config)
    started = recovery.process_start_time(data["pid"])
    return {"pid": data["pid"], "started": started} if started else None


def stop_backend(config, state, backend):
    """After the swap: SIGTERM the backend that verify_backend proved, only while the same
    process (PID and start time) reports this state directory and owns the listener.
    The recovery loop starts the new one."""
    if backend is None:
        return "not running"
    data = recovery.identity(config["port"], state)
    if not data:
        return "not running"
    if data["pid"] != backend["pid"] or recovery.process_start_time(backend["pid"]) != backend["started"]:
        raise RuntimeError("The backend changed during the install; it was not verified.")
    recovery.verify_listener(backend["pid"], config)
    os.kill(backend["pid"], signal.SIGTERM)
    deadline = time.monotonic() + BACKEND_EXIT_SECONDS
    while time.monotonic() < deadline:
        try:
            os.kill(backend["pid"], 0)
        except ProcessLookupError:
            return "stopped"
        time.sleep(.2)
    return "stopping"


def install(state, config, *, applications=Path("/Applications"), run=None, verify=verify_backend,
            stop=stop_backend, sleep=time.sleep, clock=time.time, target=signal_target, kill=os.kill):
    """Install the pending update. Returns (exit status, detail)."""
    updates, vm = load_updates(config["resources"])
    run = run or updates.run_tool
    pending = updates.read_pending(state)
    if pending is None:
        (state / updates.PENDING_FILE).unlink(missing_ok=True)
        return WAITING, "no pending update"
    if config.get("supervisorEnabled") is not True:
        # Without the supervisor a backend restart ends the agents.
        return WAITING, "waiting for supervisor mode"
    with held(state / "update-install.lock") as alone:
        if not alone:
            return WAITING, "another install runs"
        with held(vm.vm_state_dir(state) / vm.MAINTENANCE_LOCK, create=False) as free:
            if not free:
                return WAITING, "waiting for the VM maintenance lock"
            return _install(state, config, pending, updates, applications, run, verify, stop, sleep, clock,
                            target, kill)


def _install(state, config, pending, updates, applications, run, verify, stop, sleep, clock, target, kill):
    installed = applications / updates.APP_NAME
    staged = Path(pending["app"])
    following = applications / (installed.stem + ".next.app")
    previous = applications / (installed.stem + ".previous.app")
    receipt = {"id": pending["id"], "build": pending["build"], "from": pending["from"], "at": clock()}

    def finish(outcome, reason=None, **fields):
        write = {**receipt, "outcome": outcome, "reason": reason, **fields}
        if outcome == "installed":
            write["installedAt"] = clock()
        updates.write_json(state / updates.RECEIPT_FILE, write)
        if updates.read_pending(state) == pending:
            (state / updates.PENDING_FILE).unlink(missing_ok=True)
        updates.remove_tree(staged.parent)
        event(outcome=outcome, reason=reason, **fields)
        return (INSTALLED if outcome == "installed" else FAILED), reason or outcome

    def reopen():
        if window:
            run([OPEN, "-g", str(installed)], 30)

    current = updates.read_build(installed)
    if current is not None and current["builtAt"] >= pending["build"]["builtAt"]:
        return finish("obsolete", "The installed app is this build or a newer one.")
    window = False
    try:
        updates.verify_app(staged, installed, pending["build"], run)
        window = desktop_running(updates, run)
        if window:
            quit_desktop(updates, run, sleep, target(state, installed), kill)
        updates.remove_tree(following)
        result = run([updates.DITTO, str(staged), str(following)], updates.DITTO_SECONDS)
        if result.returncode != 0:
            raise updates.UpdateError("ditto could not copy the update: " + (result.stderr or "").strip()[-200:])
        updates.verify_app(following, installed, pending["build"], run)
    except (updates.UpdateError, RuntimeError, OSError, ValueError) as error:
        updates.remove_tree(following)
        reopen()
        return finish("failed", str(error)[:300] or type(error).__name__)
    try:
        backend, unverified = verify(config, state), None
    except (OSError, RuntimeError, ValueError) as error:
        # The install continues; the old backend runs until its next restart.
        backend, unverified = None, f"not restarted: {error}"[:300]
    # The swap. Never delete the installed app in place: its backend still runs from it.
    # From here this run imports no packaged module: the new build is at the installed path.
    try:
        if previous.exists():
            retired = applications / f".{installed.stem}.retired-{os.getpid()}.app"
            previous.rename(retired)
            updates.remove_tree(retired)
        installed.rename(previous)
    except OSError as error:
        updates.remove_tree(following)
        reopen()
        return finish("failed", f"The installed app could not move aside: {error}"[:300])
    try:
        following.rename(installed)
    except OSError as error:
        try:
            previous.rename(installed)
        except OSError as restore:
            return finish("failed", f"The swap failed and the previous app is at {previous}: {restore}"[:300])
        updates.remove_tree(following)
        reopen()
        return finish("failed", f"The swap failed; the previous app is back: {error}"[:300])
    try:
        backend = unverified or stop(config, state, backend)
    except (OSError, RuntimeError, ValueError) as error:
        # The new app is in place. The old backend runs until its next restart.
        backend = f"not restarted: {error}"[:300]
    reopen()
    return finish("installed", None, backend=backend, window=window)


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path, required=True)
    args = parser.parse_args(argv)
    filename = args.config.resolve()
    state = filename.parent.resolve()
    os.umask(0o077)
    config = recovery.load_config(filename, state)
    if "CODEX_AGENTS_SUPERVISOR_MODE" in os.environ:
        config["supervisorEnabled"] = os.environ["CODEX_AGENTS_SUPERVISOR_MODE"] == "1"
    code, detail = install(state, config)
    if code == WAITING:
        event(outcome="waiting", reason=detail)
    return code


if __name__ == "__main__":
    sys.exit(main())
