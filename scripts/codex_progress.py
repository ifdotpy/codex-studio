"""Read each agent's plain progress file without running code or changing state."""

from contextlib import contextmanager
import os
from pathlib import Path
import re
import stat

MAX_PROGRESS_BYTES = 128 * 1024
_AGENT_ID = re.compile(r"[A-Za-z0-9_-]{1,200}\Z")


def progress_path(state_dir, agent_id):
    if not isinstance(agent_id, str) or not _AGENT_ID.fullmatch(agent_id):
        raise ValueError("Invalid agent identity for PROGRESS.md")
    return Path(state_dir).absolute() / "progress" / agent_id / "PROGRESS.md"


@contextmanager
def _directory(state_dir, agent_id, *, create=False):
    # Open each untrusted component separately. A renamed directory or symlink
    # cannot redirect an already opened directory descriptor to another agent.
    progress_path(state_dir, agent_id)
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    descriptors = [os.open(Path(state_dir), os.O_RDONLY | os.O_DIRECTORY)]
    try:
        for name in ("progress", agent_id):
            if create:
                try:
                    os.mkdir(name, mode=0o700, dir_fd=descriptors[-1])
                except FileExistsError:
                    pass
            descriptors.append(os.open(name, flags, dir_fd=descriptors[-1]))
        yield descriptors[-1]
    finally:
        for descriptor in reversed(descriptors):
            os.close(descriptor)


def provision_progress(state_dir, agent_id):
    """Create an empty file once. Never replace existing agent content."""
    path = progress_path(state_dir, agent_id)
    with _directory(state_dir, agent_id, create=True) as directory:
        try:
            descriptor = os.open("PROGRESS.md", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                 0o600, dir_fd=directory)
        except FileExistsError:
            if not stat.S_ISREG(os.stat("PROGRESS.md", dir_fd=directory, follow_symlinks=False).st_mode):
                raise ValueError("PROGRESS.md must be a regular file")
        else:
            os.close(descriptor)
    return path


def progress_context(state_dir, agent_id):
    path = progress_path(state_dir, agent_id)
    return (
        f"Your progress file is {path}. Read and edit this plain UTF-8 Markdown file with ordinary file tools. "
        "Studio displays it above this chat's composer. Keep it at most 128 KiB. "
        "Use it for the current status, results, and blockers. Update it when these facts change. "
        "An empty or missing file clears the display. This file belongs to this agent, even when agents share a working directory. "
        "It persists across restarts. It does not run scripts, read command output, or wake the model. "
        "The orchestration_panel and orchestration_panel_feed tools are retired."
    )


def _revision(info):
    return ":".join(str(value) for value in
                    (info.st_dev, info.st_ino, info.st_mtime_ns, info.st_ctime_ns, info.st_size))


def read_progress(state_dir, agent_id):
    path = progress_path(state_dir, agent_id)
    result = {"id": agent_id, "agent": agent_id, "format": "markdown", "markdown": "",
              "path": str(path), "revision": None, "updated": None, "exists": False, "error": None}
    try:
        with _directory(state_dir, agent_id) as directory:
            descriptor = os.open("PROGRESS.md", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
            with os.fdopen(descriptor, "rb") as stream:
                result["exists"] = True
                before = os.fstat(stream.fileno())
                if not stat.S_ISREG(before.st_mode):
                    raise ValueError("PROGRESS.md must be a regular file")
                if before.st_size > MAX_PROGRESS_BYTES:
                    raise ValueError("PROGRESS.md exceeds the 128 KiB limit")
                content = stream.read(MAX_PROGRESS_BYTES + 1)
                after = os.fstat(stream.fileno())
                if len(content) > MAX_PROGRESS_BYTES:
                    raise ValueError("PROGRESS.md exceeds the 128 KiB limit")
                if _revision(before) != _revision(after):
                    raise ValueError("PROGRESS.md changed during the read. Studio will retry")
                markdown = content.decode("utf-8")
                result.update(markdown=markdown, revision=_revision(after), updated=after.st_mtime)
    except FileNotFoundError:
        pass
    except UnicodeError:
        result["error"] = "PROGRESS.md must contain valid UTF-8 text"
    except (OSError, ValueError) as error:
        result["error"] = "Cannot read PROGRESS.md: " + str(error)
    return result
