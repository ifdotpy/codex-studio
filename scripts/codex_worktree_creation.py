"""Bounded, idempotent creation of an isolated worker worktree."""

import subprocess
import time
from pathlib import Path


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
        raise ValueError('Worker worktree identity differs from its reservation; inspect the existing directory')
    # Git registers a worktree before checkout finishes. A missing tracked file
    # means the interrupted checkout must not be adopted as a ready worker.
    missing = subprocess.check_output(
        ['git', '-C', str(directory), 'ls-files', '--deleted', '-z'], timeout=120
    )
    if missing:
        raise ValueError('Worker worktree checkout is incomplete; inspect the existing directory')
    staged = subprocess.run(
        ['git', '-C', str(directory), 'diff', '--cached', '--quiet', 'HEAD', '--'],
        timeout=120, check=False, capture_output=True
    )
    if staged.returncode != 0:
        raise ValueError('Worker worktree index differs from HEAD; inspect the existing directory')
    return True


def verify_registered_worktree(repo, directory, project_directory, branch, head):
    if not _verified_checkout(Path(repo), Path(directory), Path(project_directory), branch, head):
        raise ValueError('Worker worktree registration is missing; inspect the existing directory')


def create_worker_worktree(repo, directory, project_directory, branch, *, base_commit=None,
                           run=subprocess.run, sleep=time.sleep):
    """Retry timeouts only when the exact reserved worktree is safe to retry or adopt."""
    repo, directory, project_directory = map(Path, (repo, directory, project_directory))
    head = base_commit or subprocess.check_output(
        ['git', '-C', str(repo), 'rev-parse', 'HEAD'], timeout=30
    ).decode().strip()
    for attempt, timeout in enumerate((60, 180, 300)):
        if _verified_checkout(repo, directory, project_directory, branch, head):
            return True
        if directory.is_symlink() or (directory.exists() and any(directory.iterdir())):
            raise ValueError('Worker worktree path has unregistered content; inspect the existing directory')
        branch_exists = subprocess.run(
            ['git', '-C', str(repo), 'show-ref', '--verify', '--quiet', 'refs/heads/' + branch],
            timeout=30, check=False
        ).returncode == 0
        if branch_exists:
            raise ValueError('Worker worktree branch exists without its reserved path; inspect Git worktrees')
        try:
            run(['git', '-C', str(repo), '-c',
                 'checkout.workers=' + ('16' if attempt == 0 else '4'),
                 'worktree', 'add', '-b', branch, str(directory), head],
                check=True, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            if _verified_checkout(repo, directory, project_directory, branch, head):
                return True
            if attempt == 2:
                raise
            sleep((2, 5)[attempt])
            continue
        return False
    raise AssertionError('Worktree retry loop ended without an outcome')
