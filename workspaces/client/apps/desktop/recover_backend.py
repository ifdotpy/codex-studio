#!/usr/bin/env python3
"""Keep the saved Studio backend available without taking over an occupied runtime."""
import argparse
import fcntl
from dataclasses import dataclass
from enum import StrEnum
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from desktop_process import startup_backend_birth, startup_backend_facts, recovery_interpreter_facts, STARTUP_STATE_FILES


# Supervisor protocol version that the Rust backend uses to start providers. Owner:
# workspaces/runtime/packages/runtime-providers/src/supervisor_protocol.rs (SUPERVISOR_PROTOCOL_V2);
# recovery-test.py checks this copy against it.
SUPERVISOR_PROTOCOL_V2 = 2
# Owner: MAX_SUPERVISOR_FRAME_BYTES in runtime-process/src/supervisor_contract.rs.
# recovery-test.py checks parity; includes the JSON line delimiter.
MAX_SUPERVISOR_FRAME_BYTES = 33554432


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def validate_identity(data, state):
    """Validate one desktop response before a recovery or delivery action."""
    if (data.get("application") != "codex-agents" or data.get("protocol") != 1
            or data.get("backendKind") != "rust" or data.get("stateDir") != str(state) or type(data.get("pid")) is not int
            or data["pid"] < 1):
        raise RuntimeError("The backend identity does not match this recovery service.")
    return data


def identity(port, state, timeout=5):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        with opener.open(f"http://127.0.0.1:{port}/api/desktop", timeout=timeout) as response:
            data = json.load(response)
    except urllib.error.URLError as error:
        if isinstance(error.reason, ConnectionRefusedError):
            return None
        raise
    validate_identity(data, state)
    os.kill(data["pid"], 0)
    return data


def backend_process_arguments(pid, resources):
    """Read exact native arguments through the desktop-owned process reader."""
    from desktop_process import process_launch_command
    return process_launch_command(pid)


def rust_server(config):
    """The configured Rust server, only when it is an executable file."""
    server = Path(config["runtimeServer"])
    if not server.is_file() or not os.access(server, os.X_OK):
        raise RuntimeError(f"The configured Rust server is missing or not executable: {server}")
    return server


def file_sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_backend_process(data, config):
    """Prove the reported PID is the configured backend listening on this port."""
    pid = data["pid"]
    try:
        arguments = backend_process_arguments(pid, config["resources"])
    except (OSError, ValueError, RuntimeError, ImportError) as error:
        raise RuntimeError(f"Cannot verify fallback backend PID {pid}") from error
    server = rust_server(config)
    if (data.get("backendKind") != "rust" or not arguments
            or Path(arguments[0]).resolve() != server.resolve()
            or data.get("backendBuild") != file_sha256(server)):
        raise RuntimeError("The reported backend PID is not the configured Rust server process.")
    verify_listener(pid, config)


def verify_listener(pid, config):
    """Prove that the PID owns the configured listener."""
    try:
        listeners = subprocess.check_output(
            ["/usr/sbin/lsof", "-nP", f"-iTCP:{config['port']}", "-sTCP:LISTEN", "-t"],
            text=True, stderr=subprocess.DEVNULL, timeout=2,
        ).split()
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError("Cannot verify the backend listener before fallback.") from error
    if str(pid) not in listeners:
        raise RuntimeError("The reported backend PID does not own the configured listener.")


def lease_available(state):
    # This probe is not ownership. Runtime acquires its own authoritative lease.
    with (state / "runtime.lock").open("a+") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        fcntl.flock(lease, fcntl.LOCK_UN)
        return True


def process_start_time(pid):
    try:
        value = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "lstart="],
                                        text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
        state = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "stat="],
                                        text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
    except subprocess.CalledProcessError as error:
        if error.returncode == 1:
            return None
        raise RuntimeError(f"Cannot verify fallback backend PID {pid}") from error
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError(f"Cannot verify fallback backend PID {pid}") from error
    return value if value and not state.startswith("Z") else None


def listener_pids(port):
    """Exact owners of a configured port. An empty result proves no listener, not an absent backend."""
    result = subprocess.run(
        ["/usr/sbin/lsof", "-nP", f"-iTCP:{port}", "-sTCP:LISTEN", "-t"],
        text=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=2, check=False,
    )
    if result.returncode == 1 and not result.stdout.strip():
        return []
    if result.returncode != 0 or not result.stdout.strip():
        raise RuntimeError("Cannot verify the startup backend listener")
    values = result.stdout.split()
    if any(not value.isdecimal() or int(value) < 2 for value in values):
        raise RuntimeError("The startup backend listener result is invalid")
    return sorted({int(value) for value in values})


def startup_file_identity(path):
    import stat
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
            raise RuntimeError("The startup owner file is not a protected regular file")
        return {"device": info.st_dev, "inode": info.st_ino}


def startup_lease_held(state):
    """Probe only an existing lease, without creating or replacing a state file."""
    descriptor = os.open(state / "runtime.lock", os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(lease, fcntl.LOCK_UN)
        return False


def startup_delivery_config(state, installed):
    import desktop_delivery as delivery
    if not state.is_absolute() or state != state.resolve(strict=True) or installed.is_symlink():
        raise RuntimeError("Startup delivery requires canonical state and app paths")
    config = load_config(state / "background-recovery.json", state)
    if delivery.read_private_json(state / "background-recovery.json") != config:
        raise RuntimeError("The recovery configuration changed")
    resources = installed / "Contents/Resources"
    if (config.get("backend") != "rust" or config.get("supervisorEnabled") is not True
            or Path(config["runtimeServer"]).resolve() != delivery.server_path(installed)
            or Path(config["resources"]).resolve() not in (resources, resources / "workspace")):
        raise RuntimeError("The recovery configuration does not match the installed app")
    return config


def startup_supervisor_handles(state, expected):
    """Probe protocol/state and compare live handle identity with the durable generation snapshot."""
    import desktop_delivery as delivery
    startup_file_identity(state / "supervisor.sqlite3")
    if delivery.read_handles(state) != expected:
        raise RuntimeError("The retained provider PID or generation changed")
    health = supervisor_request(state, SupervisorAction.HEALTH)
    recovery = health.get("recovery")
    if (health.get("protocol") != SUPERVISOR_PROTOCOL_V2 or health.get("stateDir") != str(state)
            or not isinstance(recovery, dict) or recovery.get("blocked")
            or recovery.get("degraded") or recovery.get("fallbackReady")
            or not isinstance(health.get("handles"), list)):
        raise RuntimeError("The startup supervisor proof is unavailable or blocked")
    handles = []
    for handle in health["handles"]:
        if not isinstance(handle, dict):
            raise RuntimeError("The startup supervisor handle is invalid")
        handles.append({key: handle.get(key) for key in ("id", "pid", "signature", "startTime")})
    if (len(handles) != len(expected)
            or {row["id"]: row["pid"] for row in handles} != {key: value[0] for key, value in expected.items()}):
        raise RuntimeError("The startup supervisor handles differ from the retained snapshot")
    return sorted(handles, key=lambda row: row["id"])


# Owner: recovery.cjs::recoveryPaths. startup-delivery-test.py checks parity.
DELIVERY_RECOVERY_LABEL_PREFIX = "local.codex.agents.recovery."
DELIVERY_RECOVERY_LABEL_HEX_LENGTH = 16
# Limits for the owned, isolated configured-interpreter proof. Units: seconds and PID-line bytes.
INTERPRETER_PROBE_SECONDS = 5
INTERPRETER_PROBE_EXIT_SECONDS = 1
INTERPRETER_PROBE_PID_BYTES = 32
# Maximum seconds for one complete exact-image scan. Real scans took 3.148 and 4.084 seconds.
# Keep a bounded 10-second inspection window; an expired scan remains unknown.
IMAGE_MAPPING_INSPECTION_SECONDS = 10
# Maximum canonical journal links checked while proving one inherited state owner. Unit: links.
INSTALLED_STATE_CHAIN_MAX_LINKS = 32


def protected_interpreter_image(path):
    import stat
    path = Path(path).resolve(strict=True)
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid not in (0, os.geteuid())
                or info.st_mode & 0o022):
            raise RuntimeError("The configured interpreter image is not protected")
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    after = path.stat()
    if (info.st_dev, info.st_ino, info.st_uid, info.st_mode) != (after.st_dev, after.st_ino, after.st_uid, after.st_mode):
        raise RuntimeError("The configured interpreter image changed during observation")
    return {"path":str(path), "device":info.st_dev, "inode":info.st_ino, "sha256":digest}


def observed_interpreter_image(pid):
    facts = recovery_interpreter_facts(pid)
    if facts.get("pid") != pid or facts.get("uid") != os.geteuid():
        raise RuntimeError("The recovery interpreter belongs to another process owner")
    image = protected_interpreter_image(facts["executable"])
    if ({key:image[key] for key in ("device", "inode")} not in facts["textFiles"]
            or startup_backend_birth(pid) != facts["birth"]):
        raise RuntimeError("The recovery interpreter image does not match its kernel mapping")
    return {"birth":facts["birth"], "image":image}


def configured_interpreter_image(configured):
    """Observe only an owned isolated probe of the exact configured launcher. Never inspect user code or environment."""
    import selectors
    launcher = protected_interpreter_image(configured)
    command = [str(configured), "-I", "-S", "-B", "-c",
               "import os,sys; print(os.getpid(),flush=True); sys.stdin.buffer.read(1)"]
    child = subprocess.Popen(command, cwd="/", env={"PATH":"/usr/bin:/bin", "LANG":"C", "LC_ALL":"C"},
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                             start_new_session=True)
    try:
        deadline = time.monotonic() + INTERPRETER_PROBE_SECONDS
        body = b""
        with selectors.DefaultSelector() as ready:
            ready.register(child.stdout, selectors.EVENT_READ)
            while not body.endswith(b"\n"):
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not ready.select(remaining):
                    raise RuntimeError("The configured interpreter probe did not become ready")
                part = os.read(child.stdout.fileno(), INTERPRETER_PROBE_PID_BYTES + 1 - len(body))
                if not part or len(body) + len(part) > INTERPRETER_PROBE_PID_BYTES:
                    raise RuntimeError("The configured interpreter probe returned an invalid PID")
                body += part
        if body != f"{child.pid}\n".encode():
            raise RuntimeError("The configured interpreter probe does not identify its owned child")
        observed = observed_interpreter_image(child.pid)
        if time.monotonic() > deadline or protected_interpreter_image(configured) != launcher:
            raise RuntimeError("The configured interpreter launcher changed during its probe")
        if protected_interpreter_image(observed["image"]["path"]) != observed["image"]:
            raise RuntimeError("The configured interpreter companion changed during its probe")
        return observed["image"]
    finally:
        child.stdin.close()
        child.stdout.close()
        try:
            child.wait(timeout=INTERPRETER_PROBE_EXIT_SECONDS)
        except subprocess.TimeoutExpired:
            child.kill()  # Only this owned child, never a PID read from metadata or probe output.
            child.wait(timeout=INTERPRETER_PROBE_EXIT_SECONDS)
            raise RuntimeError("The configured interpreter probe did not exit") from None
        if child.returncode != 0:
            raise RuntimeError("The configured interpreter probe failed")


def verify_delivery_recovery_owner(config, state, installed, *, current=False):
    """Read the existing registered job and its canonical config. Never start or stop it."""
    import re
    import plistlib
    import desktop_delivery as delivery
    label = DELIVERY_RECOVERY_LABEL_PREFIX + hashlib.sha256(str(state).encode()).hexdigest()[:DELIVERY_RECOVERY_LABEL_HEX_LENGTH]
    result = subprocess.run(["/bin/launchctl", "print", f"gui/{os.geteuid()}/{label}"],
                            text=True, capture_output=True, timeout=2, check=False)
    pids = re.findall(r"^\s*pid\s*=\s*([0-9]+)\s*$", result.stdout, re.MULTILINE)
    if result.returncode != 0 or len(pids) != 1 or int(pids[0]) < 2:
        raise RuntimeError("The existing delivery recovery job is unavailable")
    pid = int(pids[0])
    if current and pid != os.getpid():
        raise RuntimeError("Only the existing registered recovery owner can consume this intent")
    birth = startup_backend_birth(pid)
    command = backend_process_arguments(pid, config["resources"])
    expected = ["-B", str(installed / "Contents/Resources/recover_backend.py"),
                "--config", str(state / "background-recovery.json")]
    if birth is None or not command or len(command) != 5 or command[1:] != expected:
        raise RuntimeError("The registered delivery recovery job belongs to another launch")
    plist = Path.home() / "Library/LaunchAgents" / (label + ".plist")
    original_plist = delivery.read_private_bytes(plist)
    saved = plistlib.loads(original_plist)
    registered = [config["python"], *expected]
    arguments = re.findall(r"^\s*arguments = \{\s*\n(.*?)^\s*\}", result.stdout, re.MULTILINE | re.DOTALL)
    paths = re.findall(r"^\s*path = (.+?)\s*$", result.stdout, re.MULTILINE)
    if (not isinstance(saved, dict) or saved.get("Label") != label or saved.get("ProgramArguments") != registered
            or paths != [str(plist)] or len(arguments) != 1
            or [line.strip() for line in arguments[0].splitlines() if line.strip()] != registered):
        raise RuntimeError("The protected recovery registration differs from its configured launch")
    observed = observed_interpreter_image(pid)
    if (observed["birth"] != birth or configured_interpreter_image(config["python"]) != observed["image"]
            or Path(command[0]).resolve() not in {Path(config["python"]).resolve(), Path(observed["image"]["path"])}
            or observed_interpreter_image(pid) != observed
            or delivery.read_private_bytes(plist) != original_plist):
        raise RuntimeError("The registered recovery interpreter differs from its configured launcher proof")
    lock = state / "background-recovery.lock"
    startup_file_identity(lock)
    descriptor = os.open(lock, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            pass
        else:
            fcntl.flock(lease, fcntl.LOCK_UN)
            raise RuntimeError("The existing delivery recovery owner does not hold its lease")
    opened = subprocess.run(["/usr/sbin/lsof", "-nP", "-a", "-p", str(pid), "-t", str(lock)],
                            text=True, capture_output=True, timeout=2, check=False)
    if (opened.returncode != 0 or opened.stderr.strip() or opened.stdout.split() != [str(pid)]
            or startup_backend_birth(pid) != birth):
        raise RuntimeError("The existing delivery recovery job does not own its canonical lease")


def installed_delivery_origin(config, state, installed, proof, handles, *, swapped):
    """Revalidate the prior installed request and its exact artifact, without selecting a PID."""
    import desktop_delivery as delivery
    delivery.validate_installed_proof(proof)
    if (startup_delivery_config(state, installed) != config
            or delivery.request_hash(delivery_config_scope(config)) != proof["configHash"]):
        raise RuntimeError("The installed-delivery recovery configuration changed")
    prior = delivery.read_delivery_record(delivery.journal_path(state, proof["priorId"]))
    if (not prior or delivery.request_hash(prior["request"]) != proof["priorRequestHash"]
            or prior["request"]["stateDir"] != str(state)
            or prior["request"]["installed"] != str(installed)
            or prior["request"]["build"] != proof["previousBuild"]
            or prior["request"]["bundleHash"] != proof["previousBundleHash"]
            or prior["request"]["backendHash"] != proof["executable"]["sha256"]
            or prior["handles"] != handles):
        raise RuntimeError("The prior installed delivery differs from its recovery proof")
    previous = installed.with_name(installed.stem + ".previous.app") if swapped else installed
    if (previous.is_symlink() or updates_build(previous) != proof["previousBuild"]
            or delivery.bundle_hash(previous) != proof["previousBundleHash"]
            or file_sha256(delivery.server_path(previous)) != proof["executable"]["sha256"]
            or startup_file_identity(delivery.server_path(previous)) != {key: proof["executable"][key] for key in ("device", "inode")}):
        raise RuntimeError("The prior installed artifact changed")
    if startup_supervisor_handles(state, handles) != proof["supervisorHandles"]:
        raise RuntimeError("The original retained supervisor handle identity changed")
    installed_state_owner(state, installed, proof, handles)
    return delivery.server_path(previous)


def installed_state_owner(state, installed, proof, handles, *, trail=frozenset()):
    import desktop_delivery as delivery
    if proof['priorId'] in trail or len(trail) >= INSTALLED_STATE_CHAIN_MAX_LINKS:
        raise RuntimeError("The installed state-owner chain is cyclic or exceeds its bound")
    prior = delivery.read_delivery_record(delivery.journal_path(state,proof['priorId']))
    if not prior or delivery.request_hash(prior['request']) != proof['priorRequestHash']:
        raise RuntimeError("The installed state-owner predecessor changed")
    owner = proof.get("stateOwner") or {"id":proof["priorId"], "requestHash":proof["priorRequestHash"],
                                        "backendBuild":proof["executable"]["sha256"]}
    direct = {'id':proof['priorId'],'requestHash':proof['priorRequestHash'],'backendBuild':proof['executable']['sha256']}
    inherited = prior['request'].get('installedDelivery')
    if inherited is not None and proof['files'] != inherited['files']:
        raise RuntimeError("The inherited state file identities changed")
    if owner != direct:
        if inherited is None or owner != installed_state_owner(state,installed,inherited,handles,trail=trail|{proof['priorId']}):
            raise RuntimeError("The state owner does not belong to the exact predecessor chain")
    record = delivery.read_delivery_record(delivery.journal_path(state, owner["id"]))
    if (not record or delivery.request_hash(record["request"]) != owner["requestHash"]
            or record["request"]["backendHash"] != owner["backendBuild"]
            or record["request"]["stateDir"] != str(state) or record["request"]["installed"] != str(installed)
            or record["handles"] != handles):
        raise RuntimeError("The canonical state-owner journal differs from its exact chain proof")
    return owner


def delivery_config_scope(config):
    """Only fixed nonsecret launch scope enters a delivery proof. Never copy environment records."""
    if config.get("enabled") is not True:
        raise RuntimeError("Installed-delivery recovery requires its existing enabled owner")
    return {key: config.get(key) for key in (
        "version", "enabled", "stateDir", "resources", "runtimeServer", "python", "codex",
        "port", "backend", "supervisorEnabled", "processSupervisor")}


def capture_installed_delivery(config, state, installed, operation, *, run):
    import desktop_delivery as delivery
    import desktop_updates as updates
    if not delivery.valid_operation(operation):
        raise RuntimeError("The prior installed delivery identity is invalid")
    prior = delivery.read_delivery_record(delivery.journal_path(state, operation))
    receipt = delivery.read_private_json(state / updates.RECEIPT_FILE)
    if (not prior or not isinstance(receipt, dict) or prior["phase"] not in delivery.PHASES.values()
            or receipt.get("id") != operation or receipt.get("outcome") != "installed"
            or receipt.get("from") != "local-delivery" or receipt.get("build") != prior["request"]["build"]):
        raise RuntimeError("The prior delivery has no exact installed receipt")
    updates.verify_app(installed, installed, prior["request"]["build"], run)
    marker = delivery.read_private_json(state / STARTUP_STATE_OWNER_FILE)
    state_owner = {"id":operation, "requestHash":delivery.request_hash(prior["request"]),
                   "backendBuild":prior["request"]["backendHash"]}
    inherited = prior["request"].get("installedDelivery")
    if inherited is not None and {name:startup_file_identity(state/name) for name in STARTUP_STATE_FILES} != inherited['files']:
        raise RuntimeError("The inherited state file identities changed")
    if not isinstance(marker, dict) or marker.get("backendBuild") != state_owner["backendBuild"]:
        if inherited is None:
            raise RuntimeError("The installed delivery has no proven inherited state owner")
        installed_delivery_origin(config, state, installed, inherited, prior["handles"], swapped=True)
        verify_startup_state_files(state, inherited, installed_state_owner(state, installed, inherited, prior["handles"])["backendBuild"])
        state_owner = installed_state_owner(state, installed, inherited, prior["handles"])
    proof = {"version": delivery.CONTRACT["installedRecoveryVersion"], "kind": "installed-delivery",
             "priorId": operation, "priorRequestHash": delivery.request_hash(prior["request"]),
             "priorReceiptHash": delivery.request_hash(receipt),
             "configHash": delivery.request_hash(delivery_config_scope(config)),
             "stateOwner":state_owner,
             "previousBuild": prior["request"]["build"], "previousBundleHash": prior["request"]["bundleHash"],
             "executable": {**startup_file_identity(delivery.server_path(installed)),
                            "sha256": prior["request"]["backendHash"]},
             "files": {name: startup_file_identity(state / name) for name in STARTUP_STATE_FILES},
             "supervisorHandles": startup_supervisor_handles(state, prior["handles"])}
    installed_delivery_origin(config, state, installed, proof, prior["handles"], swapped=False)
    verify_startup_state_files(state, proof, state_owner["backendBuild"])
    verify_delivery_recovery_owner(config, state, installed)
    if startup_lease_held(state) or listener_pids(config["port"]):
        raise RuntimeError("Installed-delivery recovery requires an unoccupied runtime lease and listener")
    return proof, prior


def verify_installed_admission(config, state, installed, proof, handles):
    import desktop_delivery as delivery
    import desktop_updates as updates
    installed_delivery_origin(config, state, installed, proof, handles, swapped=False)
    if delivery.request_hash(delivery.read_private_json(state / updates.RECEIPT_FILE)) != proof["priorReceiptHash"]:
        raise RuntimeError("The prior installed delivery receipt changed")
    verify_startup_state_files(state, proof, installed_state_owner(state, installed, proof, handles)["backendBuild"])
    verify_delivery_recovery_owner(config, state, installed)
    if listener_pids(config["port"]):
        raise RuntimeError("The installed-delivery listener became occupied")


def installed_delivery_admission(state):
    """Fence publication with existing install, maintenance, and authoritative runtime leases."""
    from contextlib import contextmanager
    from install_update import held
    import desktop_vm as vm
    @contextmanager
    def admission():
        with held(state / "update-install.lock") as alone:
            if not alone:
                raise RuntimeError("Another install owns the delivery boundary")
            with held(vm.vm_state_dir(state) / vm.MAINTENANCE_LOCK, create=False) as free:
                if not free:
                    raise RuntimeError("VM maintenance owns the delivery boundary")
                startup_file_identity(state / "runtime.lock")
                descriptor = os.open(state / "runtime.lock", os.O_RDONLY | os.O_NOFOLLOW)
                with os.fdopen(descriptor, "rb") as lease:
                    try:
                        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError as error:
                        raise RuntimeError("A runtime owner appeared before delivery publication") from error
                    try:
                        yield
                    finally:
                        fcntl.flock(lease, fcntl.LOCK_UN)
    return admission()


def mapped_backend_pids(binary, state, *, expected=None):
    """Read only exact-image backend launches for this canonical state; never return a signal permission."""
    observed = startup_file_identity(binary)
    expected = observed if expected is None else {key: expected[key] for key in ("device", "inode")}
    if observed != expected:
        raise RuntimeError("The pinned installed-delivery executable mapping changed")
    result = subprocess.run(["/usr/sbin/lsof", "-nP", "-a", "-d", "txt", "-t", str(binary)],
                            text=True, capture_output=True, timeout=IMAGE_MAPPING_INSPECTION_SECONDS, check=False)
    if result.stderr.strip() or result.returncode not in (0, 1):
        raise RuntimeError("Cannot inspect the installed-delivery executable mappings")
    values = result.stdout.split()
    if (result.returncode == 1 and values or result.returncode == 0 and not values
            or any(not value.isdecimal() or int(value) < 2 for value in values)):
        raise RuntimeError("The installed-delivery executable mapping result is invalid")
    found = []
    for pid in sorted({int(value) for value in values}):
        try:
            facts = startup_backend_facts(pid, state)
        except (OSError, RuntimeError, ValueError):
            if startup_backend_birth(pid) is None:
                continue
            raise RuntimeError("An installed-delivery executable mapping is unknown") from None
        if facts.get("uid") != os.geteuid() or expected not in facts.get("textFiles", []):
            raise RuntimeError("The installed-delivery executable mapping changed")
        argv = facts.get("argv")
        # Owner: main.rs dispatches this exact command to check_state_readonly before serving.
        if (isinstance(argv, list) and len(argv) == 3 and argv[1] == "state-check"
                and isinstance(argv[2], str) and Path(argv[2]).is_absolute()):
            continue
        arguments = argv[1:] if isinstance(argv, list) and argv else None
        serving = arguments == [] or (arguments is not None and (
            len(arguments) == 2 and arguments[0] == "--port" and arguments[1].isdecimal()
            or len(arguments) == 1 and arguments[0].startswith("--port=") and arguments[0][7:].isdecimal()))
        environment = facts.get("environment", {})
        selected = environment.get("CODEX_AGENTS_STATE_DIR")
        if not serving or not isinstance(selected, str) or not Path(selected).is_absolute():
            raise RuntimeError("An installed-delivery executable has an unknown launch scope")
        if Path(selected).resolve(strict=True) == state:
            found.append(pid)
    if startup_file_identity(binary) != expected:
        raise RuntimeError("The installed-delivery executable changed during its mapping check")
    return found


# Owner: runtime-store/src/state_owner.rs. startup-delivery-test.py checks both copies.
STARTUP_STATE_OWNER_FILE = "rust-state-owner.json"
STARTUP_STATE_FORMAT_VERSION = 1


def verify_startup_launch_scope(config, state, installed, proof):
    """The pinned argv and fixed environment remain binding after the old process ends."""
    import desktop_delivery as delivery
    delivery.validate_startup_proof(proof)
    if startup_delivery_config(state, installed) != config:
        raise RuntimeError("The startup recovery configuration changed")
    original = delivery.server_path(installed)
    if (proof["argv"] != [str(original), "--port", str(config["port"])]
            or proof["environment"].get("CODEX_AGENTS_STATE_DIR") != str(state)
            or proof["environment"].get("CODEX_AGENTS_SUPERVISOR_MODE") != "1"
            or proof["environment"].get("CODEX_AGENTS_SUPERVISOR_FALLBACK") not in (None, "0")):
        raise RuntimeError("The recorded startup launch belongs to another scope")


def verify_startup_state_files(state, proof, backend_build):
    """Keep the pinned state file identity while its verified runtime owner changes builds."""
    import desktop_delivery as delivery
    files = {name: startup_file_identity(state / name) for name in STARTUP_STATE_FILES}
    if files != proof["files"]:
        raise RuntimeError("The pinned startup state files changed")
    marker = delivery.read_private_json(state / STARTUP_STATE_OWNER_FILE)
    if (not isinstance(marker, dict) or type(marker.get("formatVersion")) is not int
            or marker != {"runtime": "rust", "formatVersion": STARTUP_STATE_FORMAT_VERSION,
                          "backendBuild": backend_build}):
        raise RuntimeError("The startup state owner marker differs from its executable")
    return files


def verify_startup_artifact(config, state, installed, proof, handles, *, swapped):
    """Validate file/state/supervisor scope independently of process liveness."""
    import desktop_delivery as delivery
    verify_startup_launch_scope(config, state, installed, proof)
    previous = installed.with_name(installed.stem + ".previous.app") if swapped else installed
    if (previous.is_symlink() or updates_build(previous) != proof["previousBuild"]
            or delivery.bundle_hash(previous) != proof["previousBundleHash"]):
        raise RuntimeError("The pinned startup backend artifact changed")
    binary = delivery.server_path(previous)
    file_identity = {key: proof["executable"][key] for key in ("device", "inode")}
    if (startup_file_identity(binary) != file_identity
            or file_sha256(binary) != proof["executable"]["sha256"]):
        raise RuntimeError("The pinned startup executable changed")
    files = verify_startup_state_files(state, proof, proof["executable"]["sha256"])
    if startup_supervisor_handles(state, handles) != proof["supervisorHandles"]:
        raise RuntimeError("The retained supervisor handle identity changed")
    return binary, file_identity, files


def verify_startup_backend(config, state, installed, proof, handles, *, swapped):
    """Revalidate the pinned kernel process and its exact old artifact. Never infer HTTP identity."""
    import desktop_delivery as delivery
    delivery.validate_startup_proof(proof)
    pid = proof["pid"]
    if startup_backend_birth(pid) != proof["birth"]:
        raise RuntimeError("The pinned startup backend changed or ended")
    binary, file_identity, files = verify_startup_artifact(
        config, state, installed, proof, handles, swapped=swapped)
    facts = startup_backend_facts(pid, state)
    original = delivery.server_path(installed)
    if (facts.get("pid") != pid or facts.get("uid") != os.geteuid()
            or facts.get("birth") != proof["birth"] or facts.get("argv") != proof["argv"]
            or facts.get("executable") not in {str(original), str(binary)}
            or file_identity not in facts.get("textFiles", [])
            or facts.get("environment") != proof["environment"]):
        raise RuntimeError("The startup process does not match its exact launch proof")
    if (facts.get("openFiles") != files or not startup_lease_held(state)
            or listener_pids(config["port"]) != [pid]):
        raise RuntimeError("The startup backend does not own this state and listener")
    if startup_backend_birth(pid) != proof["birth"]:
        raise RuntimeError("The startup backend changed during verification")


def updates_build(app):
    import desktop_updates as updates
    return updates.read_build(app)


def capture_startup_backend(config, state, installed, pid, handles, *, run):
    import desktop_delivery as delivery
    import desktop_updates as updates
    facts = startup_backend_facts(pid, state)
    binary = delivery.server_path(installed)
    build = updates.read_build(installed)
    updates.verify_app(installed, installed, build, run)
    proof = {"version": delivery.STARTUP_INTENT_VERSION, "kind": "startup-backend", "pid": pid,
             "birth": facts["birth"], "argv": facts["argv"], "environment": facts["environment"],
             "executable": {**startup_file_identity(binary), "sha256": file_sha256(binary)},
             "files": {name: startup_file_identity(state / name) for name in STARTUP_STATE_FILES},
             "previousBuild": build, "previousBundleHash": delivery.bundle_hash(installed),
             "supervisorHandles": startup_supervisor_handles(state, handles)}
    verify_startup_backend(config, state, installed, proof, handles, swapped=False)
    return proof


def startup_replacement_ready(config, state, request, handles):
    """Observe an exact replacement started by an existing owner. This function never signals or starts it."""
    proof = request["startupBackend"]
    data = identity(config["port"], state, timeout=2)
    if data is None:
        return False
    validate_identity(data, state)
    fallback = data.get("supervisorFallback")
    if (data["pid"] == proof["pid"] or data.get("backendBuild") != request["backendHash"]
            or data.get("supervisorMode") is not True or (fallback is not None and fallback is not False)):
        raise RuntimeError("The running replacement does not match the startup delivery request")
    verify_backend_process(data, config)
    if listener_pids(config["port"]) != [data["pid"]] or not startup_lease_held(state):
        raise RuntimeError("The replacement does not own its exact listener and state lease")
    verify_startup_state_files(state, proof, request["backendHash"])
    if startup_supervisor_handles(state, handles) != proof["supervisorHandles"]:
        raise RuntimeError("The retained supervisor handle identity changed")
    if startup_backend_birth(proof["pid"]) is not None:
        raise RuntimeError("The pinned startup backend PID was reused")
    return True


def installed_delivery_tick(config, state, record):
    """The existing recovery owner handles a PID-free installed-delivery proof under its locks."""
    import desktop_delivery as delivery
    import desktop_updates as updates
    request, proof = record["request"], record["request"]["installedDelivery"]
    installed = Path(request["installed"])
    if record.get("startupRecovery") not in (None, {"phase": "stopped"}):
        raise RuntimeError("The PID-free recovery has an unknown outcome")
    verify_delivery_recovery_owner(config, state, installed, current=True)
    receipt = delivery.read_private_json(state / updates.RECEIPT_FILE)
    if not isinstance(receipt, dict):
        raise RuntimeError("The installed-delivery receipt is invalid")
    installed_result = (receipt.get("id") == record["id"] and receipt.get("outcome") == "installed"
                        and receipt.get("from") == "local-delivery" and receipt.get("build") == request["build"])
    previous = installed_delivery_origin(config, state, installed, proof, record["handles"], swapped=installed_result)
    if not installed_result:
        pending = updates.read_pending(state)
        if (delivery.request_hash(receipt) != proof["priorReceiptHash"] or pending is None
                or pending["id"] != record["id"] or pending["build"] != request["build"]
                or pending["from"] != "local-delivery"
                or delivery.bundle_hash(Path(pending["app"])) != request["bundleHash"]):
            raise RuntimeError("Installed-delivery recovery waits for its exact pending or installed receipt")
        dispatch = record.get('installerDispatch')
        if dispatch is not None and (not isinstance(dispatch,dict) or dispatch.get('phase') != 'waiting'):
            return True, "installed-delivery recovery waits for its unknown installer dispatch outcome"
        verify_startup_state_files(state, proof, installed_state_owner(state, installed, proof, record["handles"])["backendBuild"])
        return "install-pending", "installed-delivery recovery waits for the existing installer"
    if (updates.read_build(installed) != request["build"]
            or delivery.bundle_hash(installed) != request["bundleHash"]
            or file_sha256(delivery.server_path(installed)) != request["backendHash"]):
        raise RuntimeError("The installed recovery target differs from its request")
    if mapped_backend_pids(previous, state, expected=proof["executable"]):
        return True, "installed-delivery recovery waits for its previous executable to end"
    targets = mapped_backend_pids(delivery.server_path(installed), state)
    listeners = listener_pids(config["port"])
    if targets:
        data = identity(config["port"], state, timeout=2)
        if data is None:
            return True, "installed-delivery recovery waits for the running target's real HTTP identity"
        validate_identity(data, state)
        fallback = data.get("supervisorFallback")
        if (targets != [data["pid"]] or listeners != [data["pid"]]
                or data.get("backendBuild") != request["backendHash"]
                or data.get("supervisorMode") is not True or (fallback is not None and fallback is not False)
                or not startup_lease_held(state)):
            raise RuntimeError("The running target differs from its installed-delivery recovery proof")
        verify_backend_process(data, config)
        verify_startup_state_files(state, proof, request["backendHash"])
    else:
        if listeners or startup_lease_held(state):
            return True, "installed-delivery recovery waits for an unresolved runtime owner"
        marker = delivery.read_private_json(state / STARTUP_STATE_OWNER_FILE)
        build = marker.get("backendBuild") if isinstance(marker, dict) else None
        if build not in (installed_state_owner(state, installed, proof, record["handles"])["backendBuild"], request["backendHash"]):
            raise RuntimeError("The installed-delivery state marker names another runtime")
        verify_startup_state_files(state, proof, build)
        # Recheck both exact images after the lease and state observations. The old helper
        # has exited before this helper owns background-recovery.lock. No PID is signaled.
        if (mapped_backend_pids(previous, state, expected=proof["executable"])
                or mapped_backend_pids(delivery.server_path(installed), state)):
            return True, "installed-delivery recovery waits for a concurrent runtime launch"
    if record.get("startupRecovery") != {"phase": "stopped"}:
        # The CLI holds this same nonblocking lease across installer dispatch. Re-read
        # after admission so neither writer can erase the other's durable outcome.
        with delivery.delivery_lease(state):
            latest = delivery.read_delivery_record(delivery.journal_path(state,record['id']))
            if not latest or latest['request'] != request:
                raise RuntimeError("The installed recovery journal changed before settlement")
            latest["startupRecovery"] = {"phase": "stopped"}
            delivery.save_journal(state, latest)
    return False, None


def startup_delivery_tick(config, state):
    """The existing recovery owner consumes a local delivery intent before any HTTP wait."""
    import desktop_delivery as delivery
    import desktop_updates as updates
    from install_update import held
    import desktop_vm as vm
    pointer = state / delivery.STARTUP_INTENT_FILE
    if not pointer.exists() and not pointer.is_symlink():
        return False, None
    intent = delivery.read_private_json(pointer)
    if (not isinstance(intent, dict) or set(intent) != {"version", "id", "requestHash"}
            or type(intent["version"]) is not int or intent["version"] != delivery.STARTUP_INTENT_VERSION
            or not isinstance(intent["id"], str) or len(intent["id"]) != 36
            or any(c not in "0123456789abcdef-" for c in intent["id"])):
        raise RuntimeError("The startup delivery intent is invalid")
    record = delivery.read_delivery_record(delivery.journal_path(state, intent["id"]))
    if (not record or record["id"] != intent["id"]
            or delivery.request_hash(record["request"]) != intent["requestHash"]
            or record["request"]["stateDir"] != str(state)
            or not any(key in record["request"] for key in ("startupBackend", "installedDelivery"))):
        raise RuntimeError("The startup delivery intent does not match its durable request")
    recovery = record.get("startupRecovery")
    if recovery is not None and (not isinstance(recovery, dict)
                                 or set(recovery) != {"phase"}
                                 or recovery["phase"] not in ("stop-requested", "stop-sent", "stopped")):
        raise RuntimeError("The startup recovery outcome is unknown")
    with held(state / "update-install.lock") as alone:
        if not alone:
            return True, "startup delivery waits for the install owner"
        with held(vm.vm_state_dir(state) / vm.MAINTENANCE_LOCK, create=False) as free:
            if not free:
                return True, "startup delivery waits for VM maintenance"
            request = record["request"]
            installed = Path(request["installed"])
            if "installedDelivery" in request:
                return installed_delivery_tick(config, state, record)
            verify_startup_launch_scope(config, state, installed, request["startupBackend"])
            try:
                receipt = delivery.read_private_json(state / updates.RECEIPT_FILE)
            except FileNotFoundError:
                receipt = {}
            if (receipt.get("id") != record["id"] or receipt.get("outcome") != "installed"
                    or receipt.get("from") != "local-delivery" or receipt.get("build") != request["build"]):
                pending = updates.read_pending(state)
                if (pending is not None and pending["id"] == record["id"]
                        and pending["build"] == request["build"] and pending["from"] == "local-delivery"
                        and delivery.bundle_hash(Path(pending["app"])) == request["bundleHash"]):
                    proof = request["startupBackend"]
                    if startup_backend_birth(proof["pid"]) is None:
                        verify_startup_artifact(config, state, installed, proof, record["handles"], swapped=False)
                        if startup_lease_held(state) or listener_pids(config["port"]):
                            return True, "startup delivery waits for its ended backend lease and listener"
                        if startup_backend_birth(proof["pid"]) is not None:
                            raise RuntimeError("The pinned startup backend PID was reused")
                    else:
                        verify_startup_backend(config, state, installed, proof, record["handles"], swapped=False)
                    return "install-pending", "startup delivery waits for its existing install owner"
                return True, "startup delivery waits for its exact installed receipt"
            if (updates.read_build(installed) != request["build"]
                    or delivery.bundle_hash(installed) != request["bundleHash"]
                    or file_sha256(delivery.server_path(installed)) != request["backendHash"]):
                raise RuntimeError("The installed bundle differs from the startup delivery request")
            proof = request["startupBackend"]
            if recovery == {"phase": "stopped"}:
                if startup_supervisor_handles(state, record["handles"]) != proof["supervisorHandles"]:
                    raise RuntimeError("The retained supervisor handle identity changed")
                return False, None
            birth = startup_backend_birth(proof["pid"])
            if birth is None:
                if listener_pids(config["port"]):
                    if not startup_replacement_ready(config, state, request, record["handles"]):
                        return True, "startup delivery waits for the running target's exact HTTP identity"
                    record["startupRecovery"] = {"phase": "stopped"}
                    delivery.save_journal(state, record)
                    return False, None
                verify_startup_artifact(config, state, installed, proof, record["handles"], swapped=True)
                if (startup_lease_held(state) or listener_pids(config["port"])
                        or startup_supervisor_handles(state, record["handles"]) != proof["supervisorHandles"]):
                    return True, "startup delivery waits for its exact backend and lease to end"
                if startup_backend_birth(proof["pid"]) is not None:
                    raise RuntimeError("The pinned startup backend PID was reused")
                record["startupRecovery"] = {"phase": "stopped"}
                delivery.save_journal(state, record)
                return False, None
            if birth != proof["birth"]:
                raise RuntimeError("The pinned startup backend PID was reused")
            if recovery is not None:
                return True, "startup backend stop outcome remains pending"
            verify_startup_backend(config, state, installed, proof, record["handles"], swapped=True)
            record["startupRecovery"] = {"phase": "stop-requested"}
            delivery.save_journal(state, record)
            # Re-read the exact kernel birth immediately before signaling. A lost local outcome
            # leaves stop-requested durable, so a later tick cannot send another signal.
            if startup_backend_birth(proof["pid"]) != proof["birth"]:
                raise RuntimeError("The pinned startup backend changed before its stop")
            os.kill(proof["pid"], signal.SIGTERM)
            record["startupRecovery"] = {"phase": "stop-sent"}
            delivery.save_journal(state, record)
            return True, "pinned startup backend stopping after verified delivery"


def load_config(filename, state):
    config = json.loads(filename.read_text())
    if config.get("version") != 1 or Path(config["stateDir"]).resolve() != state:
        raise RuntimeError("The recovery configuration uses a different state directory.")
    if "supervisorEnabled" in config and type(config["supervisorEnabled"]) is not bool:
        raise RuntimeError("The recovery supervisor setting is invalid.")
    if type(config.get("port")) is not int or not 1024 <= config["port"] <= 65535:
        raise RuntimeError("The recovery port is invalid.")
    if config.get("backend", "rust") != "rust":
        raise RuntimeError("The recovery backend must be rust.")
    names = ["python", "codex", "resources", "runtimeServer"]
    if config.get("supervisorEnabled") is True:
        names.append("processSupervisor")
    for name in names:
        if type(config.get(name)) is not str or not Path(config[name]).is_absolute():
            raise RuntimeError(f"The recovery {name} path must be absolute.")
    return config


def launch_environment(config, state, supervisor_fallback=False):
    env = dict(os.environ)
    for key in config.get("unsetEnvironment", []):
        env.pop(key, None)
    env.update(config.get("environment", {}))
    env.update({"CODEX_AGENTS_STATE_DIR": str(state), "CODEX_BIN": config["codex"]})
    if supervisor_fallback:
        env.pop("CODEX_AGENTS_SUPERVISOR_MODE", None)
        env["CODEX_AGENTS_SUPERVISOR_FALLBACK"] = "1"
    else:
        env.pop("CODEX_AGENTS_SUPERVISOR_FALLBACK", None)
        if config.get("supervisorEnabled") is True:
            env["CODEX_AGENTS_SUPERVISOR_MODE"] = "1"
        else:
            env.pop("CODEX_AGENTS_SUPERVISOR_MODE", None)
    return env


def supervisor_tick(config, state, child=None):
    """Probe the separately supervised owner without taking over its lifecycle."""
    if config.get("supervisorEnabled") is not True:
        return child, "disabled"
    binary = Path(config.get("processSupervisor") or "")
    if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError("Supervisor mode is enabled but the Rust process supervisor is missing.")
    try:
        health = supervisor_request(state, SupervisorAction.HEALTH)
    except OSError:
        # Failed probes never authorize replacing the separately supervised owner.
        return child, "supervisor starting"
    recovery = health.get("recovery") or {}
    if recovery.get("blocked"):
        raise RuntimeError(recovery["blocked"])
    if recovery.get("degraded") and recovery.get("fallbackReady"):
        return child, "supervisor degraded"
    return child, "supervisor ready"


# Seconds allowed for an administrative supervisor exchange.
SUPERVISOR_REQUEST_SECONDS = 2


@dataclass(frozen=True)
class SupervisorHello:
    ok: bool = False
    protocol: int | None = None
    error: str | None = None


@dataclass(frozen=True)
class SupervisorReply:
    requestId: int | None = None
    result: dict[str, object] | None = None
    error: str | None = None


class SupervisorAction(StrEnum):
    HEALTH = "health"
    FINISH_FALLBACK = "finishFallback"


def supervisor_request(state: Path, action: SupervisorAction) -> dict[str, object]:
    """Use the Rust supervisor's administrative socket without taking ownership."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(SUPERVISOR_REQUEST_SECONDS)
        client.connect(str(state / "supervisor.sock"))
        with client.makefile("rwb") as stream:
            def exchange(message: dict[str, object]) -> dict[str, object]:
                stream.write((json.dumps(message) + "\n").encode())
                stream.flush()
                line = stream.readline(MAX_SUPERVISOR_FRAME_BYTES + 1)
                if not line.endswith(b"\n") or len(line) > MAX_SUPERVISOR_FRAME_BYTES:
                    raise RuntimeError("The supervisor reply exceeds its limit or is incomplete.")
                return json.loads(line)

            role = "operator" if action is SupervisorAction.FINISH_FALLBACK else "probe"
            hello = SupervisorHello(**exchange({"protocol": SUPERVISOR_PROTOCOL_V2,
                              "stateDir": str(state), role: True}))
            if hello.ok is not True or hello.protocol != SUPERVISOR_PROTOCOL_V2:
                raise RuntimeError("The running process supervisor does not serve protocol 2.")
            reply = SupervisorReply(**exchange({"requestId": 1, "action": action}))
            if reply.requestId != 1 or reply.error is not None or reply.result is None:
                raise RuntimeError(reply.error or "The supervisor reply does not match the request.")
            return reply.result


def tick(config, state, child=None, supervisor_fallback=False):
    if child is not None and child.poll() is None:
        return child, "starting"
    existing = identity(config["port"], state)
    if existing and supervisor_fallback and existing.get("supervisorFallback") is True:
        return None, "fallback attached"
    if existing and supervisor_fallback and existing.get("supervisorMode") is True:
        verify_backend_process(existing, config)
        os.kill(existing["pid"], signal.SIGTERM)
        return None, "supervisor backend stopping for fallback"
    if existing:
        return None, "attached"
    if not lease_available(state):
        return None, "waiting for runtime owner"
    command = [str(rust_server(config)), "--port", str(config["port"])]
    env = launch_environment(config, state, supervisor_fallback=supervisor_fallback)
    # A fresh Rust state accepts only a private canvas.log (accepted_private_file in
    # runtime-store fresh_state.rs); launchd starts this agent with umask 022.
    log_fd = os.open(state / "canvas.log", os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    with os.fdopen(log_fd, "ab", buffering=0) as log:
        child = subprocess.Popen(command,
                                 cwd=Path.home(), env=env, stdin=subprocess.DEVNULL,
                                 stdout=log, stderr=log, start_new_session=True)
    return child, "started"


def desktop_tick(config, state, child=None):
    """Restore only a saved open-window intent whose exact previous process ended."""
    if child is not None and child.poll() is None:
        return child, "desktop running"
    filename = state / "desktop-recovery.json"
    if not filename.exists():
        return None, "desktop closed"
    intent = json.loads(filename.read_text())
    if intent.get("version") != 1 or intent.get("desiredOpen") is not True:
        return None, "desktop closed"
    pid = intent.get("pid")
    if type(pid) is not int or pid < 1 or not intent.get("startedAt"):
        raise RuntimeError("The saved desktop identity is invalid.")
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        pass
    else:
        started = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "lstart="], text=True, env={**os.environ, "LC_ALL": "C"}).strip()
        if not started:
            raise RuntimeError("The saved desktop process identity is unavailable.")
        if started == intent["startedAt"]:
            return None, "desktop attached"
    executable = Path(intent["executable"])
    profile = Path(intent["profile"])
    if not executable.is_absolute() or not executable.is_file() or not profile.is_absolute():
        raise RuntimeError("The saved desktop installation or profile is unavailable.")
    # Re-read the intent after probing. An explicit close can withdraw this request.
    if json.loads(filename.read_text()) != intent:
        return None, "desktop intent changed"
    env = launch_environment(config, state)
    env.update({"CODEX_DESKTOP_PORT": str(config["port"]), "CODEX_DESKTOP_PROFILE": str(profile)})
    with (state / "desktop-recovery.log").open("ab", buffering=0) as log:
        child = subprocess.Popen([str(executable), "--background-recovery"],
                                 cwd=Path.home(), env=env, stdin=subprocess.DEVNULL,
                                 stdout=log, stderr=log, start_new_session=True)
    return child, "desktop started"


# Seconds between two installer runs while a staged update waits (docs/peer-updates.md).
UPDATE_RETRY_SECONDS = 60
# Seconds that one installer run may take.
UPDATE_INSTALL_SECONDS = 1800


def update_tick(filename, config, state, run=subprocess.run):
    """Run the installer of this app when the backend staged an update from a paired server."""
    from desktop_updates import PENDING_FILE
    if not (state / PENDING_FILE).is_file():
        return None
    installer = Path(__file__).resolve().with_name("install_update.py")
    if not installer.is_file():
        return "update installer missing"
    result = run([config["python"], "-B", str(installer), "--config", str(filename)],
                 cwd=Path.home(), env=launch_environment(config, state), stdin=subprocess.DEVNULL,
                 timeout=UPDATE_INSTALL_SECONDS, check=False)
    return {0: "update installed", 75: "update waiting"}.get(result.returncode, "update failed")


def own_code():
    """The bytes of this agent's files in the installed app, or None when they are unreadable."""
    try:
        here = Path(__file__).resolve()
        return tuple(path.read_bytes() for path in (here, here.with_name("install_update.py")) if path.exists())
    except OSError:
        return None


def run(filename, interval=5):
    os.umask(0o077)
    state = filename.parent.resolve()
    stopping = False
    # A manual install replaces the app under this running agent. Its code stays old in memory
    # until launchd starts it again, so it exits when its files changed.
    started_code = own_code()

    def stop(*_):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    with (state / "background-recovery.lock").open("a+") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return
        child = None
        supervisor = None
        desktop = None
        fallback_pid = None
        fallback_start = None
        desktop_attempt = 0
        update_attempt = 0
        update_status = None
        previous = None
        while not stopping:
            try:
                config = load_config(filename, state)
                if config.get("enabled") is not True:
                    return
                if "CODEX_AGENTS_SUPERVISOR_MODE" in os.environ:
                    config["supervisorEnabled"] = (
                        os.environ["CODEX_AGENTS_SUPERVISOR_MODE"] == "1"
                    )
                startup_wait, startup_status = startup_delivery_tick(config, state)
                if startup_wait:
                    if startup_wait == "install-pending" and time.monotonic() >= update_attempt:
                        update_attempt = time.monotonic() + UPDATE_RETRY_SECONDS
                        update_status = update_tick(filename, config, state)
                        if update_status == "update installed":
                            print(json.dumps({"event": "backend-recovery", "status": update_status}), flush=True)
                            return
                    raise RuntimeError(startup_status)
                supervisor, supervisor_status = supervisor_tick(config, state, supervisor)
                if supervisor_status == "supervisor degraded":
                    existing = identity(config["port"], state)
                    fallback_exited = (fallback_pid is not None and fallback_start is not None
                                       and process_start_time(fallback_pid) != fallback_start)
                    if fallback_exited:
                        supervisor_request(state, SupervisorAction.FINISH_FALLBACK)
                        fallback_pid = fallback_start = None
                        supervisor_status = "supervisor ready"
                        child, status = tick(config, state, None)
                    elif existing and existing.get("supervisorFallback") is True:
                        fallback_pid = existing["pid"]
                        fallback_start = process_start_time(fallback_pid)
                        status = "fallback backend attached"
                    elif existing and existing.get("supervisorMode") is True:
                        verify_backend_process(existing, config)
                        os.kill(existing["pid"], signal.SIGTERM)
                        status = "supervisor backend stopping for fallback"
                    elif child is not None and child.poll() is None:
                        status = "fallback backend starting"
                    else:
                        child, status = tick(config, state, child, supervisor_fallback=True)
                        if status == "started":
                            fallback_pid = child.pid
                            fallback_start = process_start_time(fallback_pid)
                elif config.get("supervisorEnabled") is True and supervisor_status != "supervisor ready":
                    status = supervisor_status
                else:
                    child, status = tick(config, state, child)
                    if supervisor_status == "supervisor ready":
                        status = f"{status}; {supervisor_status}"
                if time.monotonic() >= desktop_attempt:
                    desktop, desktop_status = desktop_tick(config, state, desktop)
                    if desktop_status == "desktop started":
                        desktop_attempt = time.monotonic() + 60
                    status = f"{status}; {desktop_status}"
                if time.monotonic() >= update_attempt:
                    update_attempt = time.monotonic() + UPDATE_RETRY_SECONDS
                    update_status = None
                    update_status = update_tick(filename, config, state)
                    if update_status == "update installed":
                        # launchd starts this agent again from the new app; it starts the backend.
                        print(json.dumps({"event": "backend-recovery", "status": update_status}), flush=True)
                        return
                if update_status:
                    status = f"{status}; {update_status}"
            except (OSError, ValueError, TypeError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
                # An unavailable or uncertain owner never authorizes a replacement.
                status = f"waiting: {error}"
            if status != previous:
                print(json.dumps({"event": "backend-recovery", "status": status}), flush=True)
                previous = status
            current_code = own_code()
            if started_code is not None and current_code is not None and current_code != started_code:
                # launchd starts this agent again from the installed app; the backend keeps running.
                print(json.dumps({"event": "backend-recovery", "status": "agent code changed"}), flush=True)
                return
            deadline = time.monotonic() + interval
            while not stopping and time.monotonic() < deadline:
                time.sleep(min(.2, max(0, deadline - time.monotonic())))
    # Disabling this service does not terminate the backend or its active work.


def check_interpreter():
    """Prove that this interpreter runs the recovery job: the imports above (StrEnum needs
    Python 3.11) and the helpers shipped beside this file."""
    import desktop_process  # noqa: F401
    import desktop_updates  # noqa: F401
    import desktop_vm  # noqa: F401
    import desktop_delivery  # noqa: F401
    print(json.dumps({"python": sys.executable, "version": list(sys.version_info[:3])}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path)
    parser.add_argument("--supervisor-health", action="store_true")
    parser.add_argument("--state", type=Path)
    parser.add_argument("--check-interpreter", action="store_true")
    args = parser.parse_args()
    if args.check_interpreter:
        check_interpreter()
    elif args.supervisor_health:
        if args.state is None:
            parser.error("--supervisor-health requires --state")
        print(json.dumps(supervisor_request(args.state.resolve(), SupervisorAction.HEALTH)))
    elif args.config is None:
        parser.error("--config is required")
    else:
        run(args.config.resolve())
