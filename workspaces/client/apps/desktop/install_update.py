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


def load_recovery():
    spec = importlib.util.spec_from_file_location("studio_recover_backend", HERE / "recover_backend.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


recovery = load_recovery()


def load_updates(resources):
    """Import the shared update checks from the backend scripts of this installation."""
    scripts = str(recovery.backend_scripts(resources).resolve())
    if scripts not in sys.path:
        sys.path.insert(0, scripts)
    import codex_peer_updates
    import codex_linux_vm
    for module in (codex_peer_updates, codex_linux_vm):
        if Path(module.__file__).resolve().parent != Path(scripts):
            raise RuntimeError("The packaged update checks are unavailable.")
    return codex_peer_updates, codex_linux_vm


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


def quit_desktop(updates, run, sleep):
    result = run([OSASCRIPT, "-e", f'tell application id "{updates.BUNDLE_ID}" to quit'], QUIT_SECONDS)
    if result.returncode != 0:
        raise RuntimeError("The desktop window did not quit: " + (result.stderr or "").strip()[-200:])
    deadline = time.monotonic() + QUIT_SECONDS
    while desktop_running(updates, run):
        if time.monotonic() >= deadline:
            raise RuntimeError("The desktop window did not quit.")
        sleep(1)


def stop_backend(config, state):
    """SIGTERM the verified backend of this state directory. The recovery loop starts the new one."""
    data = recovery.identity(config["port"], state)
    if not data:
        return "not running"
    recovery.verify_backend_process(data, config)
    os.kill(data["pid"], signal.SIGTERM)
    deadline = time.monotonic() + BACKEND_EXIT_SECONDS
    while time.monotonic() < deadline:
        try:
            os.kill(data["pid"], 0)
        except ProcessLookupError:
            return "stopped"
        time.sleep(.2)
    return "stopping"


def install(state, config, *, applications=Path("/Applications"), run=None, stop=stop_backend,
            sleep=time.sleep, clock=time.time):
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
            return _install(state, config, pending, updates, applications, run, stop, sleep, clock)


def _install(state, config, pending, updates, applications, run, stop, sleep, clock):
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
            quit_desktop(updates, run, sleep)
        updates.remove_tree(following)
        result = run([updates.DITTO, str(staged), str(following)], updates.DITTO_SECONDS)
        if result.returncode != 0:
            raise updates.UpdateError("ditto could not copy the update: " + (result.stderr or "").strip()[-200:])
        updates.verify_app(following, installed, pending["build"], run)
    except (updates.UpdateError, RuntimeError, OSError, ValueError) as error:
        updates.remove_tree(following)
        reopen()
        return finish("failed", str(error)[:300] or type(error).__name__)
    # The swap. Never delete the installed app in place: its backend still runs from it.
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
        backend = stop(config, state)
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
