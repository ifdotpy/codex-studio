"""Resolve immutable bases for managed worker worktrees."""

import subprocess


def _git(repo, *args):
    return subprocess.run(
        ['git', '-C', str(repo), *args], capture_output=True, text=True,
        check=False, timeout=30,
    )


def resolve_worker_base(repo, ref=None):
    """Return the selected commit and optional main-branch lag details."""
    selected_ref = ref or 'HEAD'
    if not isinstance(selected_ref, str) or not selected_ref.strip() or len(selected_ref) > 1024:
        raise ValueError('base_ref must be a branch, tag, or commit')
    resolved = _git(repo, 'rev-parse', '--verify', '--end-of-options',
                    selected_ref.strip() + '^{commit}')
    if resolved.returncode:
        raise ValueError('base_ref does not resolve to a commit: ' + selected_ref.strip())
    commit = resolved.stdout.strip()
    target_ref = None
    upstream = _git(repo, 'rev-parse', '--verify', '--quiet',
                    'main@{upstream}^{commit}')
    if upstream.returncode == 0:
        target_ref = 'main@{upstream}'
        target = upstream.stdout.strip()
    else:
        symbolic = _git(repo, 'symbolic-ref', '--quiet', '--short', 'refs/remotes/origin/HEAD')
        candidates = [symbolic.stdout.strip()] if symbolic.returncode == 0 else []
        candidates.extend(['origin/main', 'main'])
        target = None
        for candidate in candidates:
            result = _git(repo, 'rev-parse', '--verify', '--quiet', '--end-of-options',
                          candidate + '^{commit}')
            if result.returncode == 0:
                target_ref, target = candidate, result.stdout.strip()
                break
    behind = None
    if target_ref and target != commit:
        count = _git(repo, 'rev-list', '--count', commit + '..' + target)
        if count.returncode == 0:
            behind = int(count.stdout.strip())
    return {'baseRef': selected_ref.strip(), 'baseCommit': commit,
            'mainRef': target_ref, 'behindMain': behind}
