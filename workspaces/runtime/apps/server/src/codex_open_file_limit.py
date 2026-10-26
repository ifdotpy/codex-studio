"""Raise the soft open-file limit before native processes start."""
import os


def raise_open_file_limit(target=65536):
    """Raise the soft open-file limit before native processes start.

    launchd starts the backend and the process supervisor with a soft limit of
    256. Each loaded Codex thread keeps pipes to its MCP servers, so a busy
    app-server reached that limit and could not start commands (OS error 24).
    The supervisor owns the native app-server processes, so it must raise the
    limit too; children inherit it. The hard limit is never changed.
    """
    if os.name == "nt":
        return None
    import resource
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    wanted = target if hard == resource.RLIM_INFINITY else min(target, hard)
    if soft != resource.RLIM_INFINITY and soft < wanted:
        try:
            resource.setrlimit(resource.RLIMIT_NOFILE, (wanted, hard))
        except (ValueError, OSError):
            pass
    return resource.getrlimit(resource.RLIMIT_NOFILE)[0]
