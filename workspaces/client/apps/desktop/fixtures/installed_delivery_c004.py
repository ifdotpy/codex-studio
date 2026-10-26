# Frozen compatibility consumer from c004c9f8956a6dd08f62ecb0294868eb9e19be50:workspaces/client/apps/desktop/recover_backend.py
# Test fixture only. The installed v1 owner cannot consume a v2 inherited marker.
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
        verify_startup_state_files(state, proof, proof["executable"]["sha256"])
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
        if build not in (proof["executable"]["sha256"], request["backendHash"]):
            raise RuntimeError("The installed-delivery state marker names another runtime")
        verify_startup_state_files(state, proof, build)
        # Recheck both exact images after the lease and state observations. The old helper
        # has exited before this helper owns background-recovery.lock. No PID is signaled.
        if (mapped_backend_pids(previous, state, expected=proof["executable"])
                or mapped_backend_pids(delivery.server_path(installed), state)):
            return True, "installed-delivery recovery waits for a concurrent runtime launch"
    if record.get("startupRecovery") != {"phase": "stopped"}:
        record["startupRecovery"] = {"phase": "stopped"}
        delivery.save_journal(state, record)
    return False, None
