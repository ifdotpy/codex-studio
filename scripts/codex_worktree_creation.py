"""Bounded, idempotent creation of an isolated worker worktree."""

import os
import signal
import subprocess
import tempfile
import time
from pathlib import Path


class WorktreeNeedsReview(ValueError):
    """An existing worktree needs a lead decision before its worker starts."""


def _run_checkout(command, *, timeout, check, capture_output, text):
    """Stop Git's checkout workers too when the deadline expires."""
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=text, start_new_session=True) as process:
        try:
            output, error = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate()
            raise subprocess.TimeoutExpired(command, timeout) from None
    result = subprocess.CompletedProcess(command, process.returncode, output, error)
    if check:
        result.check_returncode()
    return result


def _entry(repo, directory):
    listing = subprocess.check_output(
        ['git', '-C', str(repo), 'worktree', 'list', '--porcelain', '-z'], timeout=30
    ).decode('utf-8', errors='surrogateescape')
    for block in listing.split('\0\0'):
        fields = dict(line.split(' ', 1) for line in block.split('\0') if ' ' in line)
        if fields.get('worktree') and Path(fields['worktree']).resolve() == directory.resolve():
            return fields
    return None


def _verified_checkout(repo, directory, project_directory, branch, head):
    entry = _entry(repo, directory)
    if entry is None:
        return False
    if (entry.get('branch') != 'refs/heads/' + branch or entry.get('HEAD') != head
            or not project_directory.is_dir()):
        raise WorktreeNeedsReview(_recovery_message(directory, 'identity differs from its reservation'))
    # Git registers a worktree before checkout finishes. A missing tracked file
    # means the interrupted checkout must not be adopted as a ready worker.
    missing = subprocess.check_output(
        ['git', '-C', str(directory), 'ls-files', '--deleted', '-z'], timeout=120
    )
    if missing:
        raise WorktreeNeedsReview(_recovery_message(directory, 'checkout is incomplete'))
    staged = subprocess.run(
        ['git', '-C', str(directory), 'diff', '--cached', '--quiet', 'HEAD', '--'],
        timeout=120, check=False, capture_output=True
    )
    if staged.returncode != 0:
        raise WorktreeNeedsReview(_recovery_message(directory, 'index differs from HEAD'))
    return True


def _recovery_message(directory, reason):
    return (f'Worker worktree {reason} at {directory}. Inspect its files and Git status, '
            'repair the checkout, then resume this worker. Keep this directory until reviewed.')


def _remove_own_partial(repo, directory, branch, head):
    """Remove only this call's registered, no-index checkout with no foreign paths."""
    entry = _entry(repo, directory)
    if not entry or entry.get('HEAD') != head or entry.get('branch') != 'refs/heads/' + branch:
        return False
    index = subprocess.check_output(
        ['git', '-C', str(directory), 'rev-parse', '--path-format=absolute', '--git-path', 'index'],
        timeout=30
    ).decode().strip()
    if Path(index).exists():
        return False
    tracked = set(subprocess.check_output(
        ['git', '-C', str(repo), 'ls-tree', '-r', '-z', '--name-only', head], timeout=120
    ).decode('utf-8', errors='surrogateescape').split('\0'))
    folders = {'.'}
    for name in tracked:
        parent = Path(name).parent
        while str(parent) != '.':
            folders.add(str(parent))
            parent = parent.parent
    for root, dirs, files in os.walk(directory, followlinks=False):
        relative = Path(root).relative_to(directory)
        dirs[:] = [name for name in dirs if not (relative == Path('.') and name == '.git')]
        if any(str(relative / name) not in folders for name in dirs):
            return False
        for name in files:
            if relative == Path('.') and name == '.git':
                continue
            if str(relative / name) not in tracked:
                return False
    # Compare present files through Git's clean filters. Missing files are the
    # interrupted checkout; any changed or unexpected file belongs to review.
    with tempfile.TemporaryDirectory() as temporary:
        environment = {**os.environ, 'GIT_INDEX_FILE': str(Path(temporary) / 'index')}
        subprocess.run(['git', '-C', str(directory), 'read-tree', head],
                       env=environment, check=True, capture_output=True, timeout=120)
        status = subprocess.check_output(
            ['git', '-C', str(directory), 'status', '--porcelain=v1', '-z',
             '--untracked-files=all'], env=environment, timeout=900
        )
        if any(row and not row.startswith(b' D ') for row in status.split(b'\0')):
            return False
    _run_checkout(['git', '-C', str(repo), 'worktree', 'remove', '--force', str(directory)],
                  check=True, capture_output=True, text=True, timeout=900)
    if subprocess.run(['git', '-C', str(repo), 'rev-parse', branch],
                      capture_output=True, timeout=30).stdout.decode().strip() == head:
        subprocess.run(['git', '-C', str(repo), 'branch', '-D', branch],
                       check=True, capture_output=True, text=True, timeout=30)
    return True


def _finish_new_checkout(repo, directory, project_directory, branch, head, *, created_here):
    """Finish only a registration made by this call."""
    entry = _entry(repo, directory)
    if entry is None:
        return False
    if entry.get('branch') != 'refs/heads/' + branch or entry.get('HEAD') != head:
        raise WorktreeNeedsReview(_recovery_message(directory, 'identity differs from its reservation'))
    index = subprocess.check_output(
        ['git', '-C', str(directory), 'rev-parse', '--path-format=absolute', '--git-path', 'index'],
        timeout=30
    ).decode().strip()
    if Path(index).exists():
        # A completed checkout hook may have staged user changes. Preserve them.
        _verified_checkout(repo, directory, project_directory, branch, head)
        return True
    # A timed-out worktree add can register HEAD and copy files without writing
    # the index. This path was empty before our add, so its tracked files are ours.
    for timeout in (900, 1800):
        try:
            _run_checkout(['git', '-C', str(directory), '-c', 'checkout.workers=16',
                           'reset', '--hard', head],
                          check=True, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            continue
        except subprocess.CalledProcessError as error:
            if created_here and _remove_own_partial(repo, directory, branch, head):
                raise WorktreeNeedsReview(
                    f'Worker checkout failed. Studio removed its new partial folder at {directory}. '
                    'Check disk space, then send this worker to retry.') from error
            raise WorktreeNeedsReview(_recovery_message(directory, 'checkout failed')) from error
        return _verified_checkout(repo, directory, project_directory, branch, head)
    if created_here and _remove_own_partial(repo, directory, branch, head):
        raise WorktreeNeedsReview(
            f'Worker checkout timed out. Studio removed its new partial folder at {directory}. '
            'Free disk space, then send this worker to retry.')
    raise WorktreeNeedsReview(_recovery_message(directory, 'checkout timed out twice'))


def verify_registered_worktree(repo, directory, project_directory, branch, head):
    if not _verified_checkout(Path(repo), Path(directory), Path(project_directory), branch, head):
        raise WorktreeNeedsReview(_recovery_message(directory, 'registration is missing'))


def create_worker_worktree(repo, directory, project_directory, branch, *, base_commit=None,
                           run=_run_checkout, sleep=time.sleep):
    """Retry timeouts only when the exact reserved worktree is safe to retry or adopt."""
    repo, directory, project_directory = map(Path, (repo, directory, project_directory))
    head = base_commit or subprocess.check_output(
        ['git', '-C', str(repo), 'rev-parse', 'HEAD'], timeout=30
    ).decode().strip()
    for attempt, timeout in enumerate((900, 1800)):
        if _verified_checkout(repo, directory, project_directory, branch, head):
            return True
        if directory.is_symlink() or (directory.exists() and any(directory.iterdir())):
            raise WorktreeNeedsReview(_recovery_message(directory, 'path has unregistered content'))
        branch_exists = subprocess.run(
            ['git', '-C', str(repo), 'show-ref', '--verify', '--quiet', 'refs/heads/' + branch],
            timeout=30, check=False
        ).returncode == 0
        if branch_exists:
            raise WorktreeNeedsReview(_recovery_message(directory, 'branch exists without its reserved path'))
        created_here = not directory.exists()
        try:
            run(['git', '-C', str(repo), '-c',
                 'checkout.workers=' + ('16' if attempt == 0 else '4'),
                 'worktree', 'add', '-b', branch, str(directory), head],
                check=True, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                if _verified_checkout(repo, directory, project_directory, branch, head):
                    return True
            except ValueError:
                pass
            if _finish_new_checkout(repo, directory, project_directory, branch, head,
                                    created_here=created_here):
                return True
            if attempt == 1:
                raise WorktreeNeedsReview(_recovery_message(directory, 'creation timed out twice'))
            sleep(2)
            continue
        except subprocess.CalledProcessError as error:
            if created_here and _remove_own_partial(repo, directory, branch, head):
                raise WorktreeNeedsReview(
                    f'Worker checkout failed. Studio removed its new partial folder at {directory}. '
                    'Check disk space, then send this worker to retry.') from error
            raise WorktreeNeedsReview(_recovery_message(directory, 'creation failed')) from error
        try:
            if _verified_checkout(repo, directory, project_directory, branch, head):
                return False
        except ValueError:
            if _finish_new_checkout(repo, directory, project_directory, branch, head,
                                    created_here=created_here):
                return True
            raise
        raise WorktreeNeedsReview(_recovery_message(directory, 'registration is missing after creation'))
    raise AssertionError('Worktree retry loop ended without an outcome')
