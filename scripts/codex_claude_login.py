"""Native Claude subscription login. Studio never owns OAuth credentials."""
import copy
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import sys
import threading
import time
from urllib.parse import urlparse
import uuid

import codex_claude

ACTIVE = {'starting', 'pending'}
_LOCK = threading.Lock()


def manager(runtime):
    with _LOCK:
        if not hasattr(runtime, '_claude_login'):
            runtime._claude_login = LoginManager(runtime)
        return runtime._claude_login


def request_id(value):
    try:
        if not isinstance(value, str) or str(uuid.UUID(value)) != value:
            raise ValueError()
    except (ValueError, AttributeError):
        raise ValueError('Supply a UUID request_id') from None
    return value


def verification_url(value):
    parsed = urlparse(value)
    return (parsed.scheme == 'https' and parsed.netloc == 'claude.com'
            and parsed.path == '/cai/oauth/authorize' and not parsed.fragment)


class LoginManager:
    def __init__(self, runtime, deadline=600):
        self.runtime = runtime
        self.directory = Path(runtime.root) / 'claude-logins'
        self.directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.lock = threading.RLock()
        self.jobs = {}
        self.deadline = deadline

    def _path(self, rid):
        return self.directory / (request_id(rid) + '.json')

    def _save(self, job):
        # Browser URLs and authorization codes never enter persistent receipts.
        receipt = {k: v for k, v in job['receipt'].items() if k != 'verificationUrl'}
        path = self._path(receipt['requestId'])
        temporary = path.with_suffix('.tmp')
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, 'w') as stream:
            json.dump(receipt, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)

    def _job(self, rid):
        request_id(rid)
        if rid not in self.jobs:
            try:
                receipt = json.loads(self._path(rid).read_text())
            except FileNotFoundError:
                raise ValueError('Unknown Claude sign-in request') from None
            job = {'receipt': receipt}
            self.jobs[rid] = job
            if receipt['status'] in ACTIVE:
                self._finish(job, 'error', 'Studio restarted during sign-in. Start a new sign-in request.')
        return self.jobs[rid]

    def _finish(self, job, status, error=None):
        job['receipt'].update(status=status)
        job['receipt'].pop('verificationUrl', None)
        if error:
            job['receipt']['error'] = error
        self._save(job)

    def status(self, rid):
        with self.lock:
            return dict(self._job(rid)['receipt'])

    def start(self, key, rid):
        request_id(rid)
        with self.lock:
            if rid in self.jobs or self._path(rid).exists():
                job = self._job(rid)
                if job['receipt']['accountKey'] != key:
                    raise ValueError('This request belongs to a different account')
                return dict(job['receipt'])
            if any(job['receipt']['accountKey'] == key and job['receipt']['status'] in ACTIVE
                   for job in self.jobs.values()):
                raise ValueError('Claude sign-in is already active for this account')
            # Read the saved identity without the blocking native status command.
            with self.runtime.accounts.lock:
                profile = copy.deepcopy(self.runtime.accounts._row(key))
            if profile.get('provider') != 'claude' or profile.get('disconnected'):
                raise ValueError('Select a connected Claude account')
            email = profile.get('email')
            if not email or profile.get('accountId') != 'claude:' + email:
                raise ValueError('The Claude account has no saved identity')
            env = codex_claude.subscription_env(profile)
            config = str(Path(env.get('CLAUDE_CONFIG_DIR') or Path.home() / '.claude').expanduser().resolve())
            digest = hashlib.sha256(config.encode()).hexdigest()
            lease = open(self.directory / (digest + '.lock'), 'a')
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                lease.close()
                raise ValueError('Claude sign-in is already active for this configuration') from None
            job = {'receipt': {'requestId': rid, 'accountKey': key, 'status': 'starting', 'email': email},
                   'profile': profile, 'lease': lease, 'codeSent': False}
            self.jobs[rid] = job
            try:
                self._save(job)
            except Exception:
                lease.close()
                del self.jobs[rid]
                raise
            threading.Thread(target=self._run, args=(job, env), daemon=True).start()
            return dict(job['receipt'])

    def code(self, rid, code):
        if not isinstance(code, str) or not 1 <= len(code) <= 4096 or not code.isprintable():
            raise ValueError('Paste a valid authorization code')
        with self.lock:
            job = self._job(rid)
            if job.get('codeSent') or job['receipt']['status'] not in ACTIVE:
                return dict(job['receipt'])
            process = job.get('process')
            if not process or not job['receipt'].get('verificationUrl') or process.poll() is not None:
                raise ValueError('Wait for the Claude sign-in link')
            # Record intent before touching the pipe. An uncertain write is never replayed.
            job['codeSent'] = True
            try:
                process.stdin.write((code + '\n').encode())
                process.stdin.flush()
            except (OSError, ValueError):
                self._finish(job, 'error', 'Cannot confirm delivery of the code. Start a new sign-in request.')
                self._stop(job)
            return dict(job['receipt'])

    def _stop(self, job):
        process = job.get('process')
        if process and process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass

    def cancel(self, rid):
        with self.lock:
            job = self._job(rid)
            if job['receipt']['status'] in ACTIVE:
                self._finish(job, 'cancelled')
                self._stop(job)
            return dict(job['receipt'])

    def _run(self, job, env):
        process = None
        try:
            binary = codex_claude.installed(job['profile'])
            if not binary:
                raise ValueError('missing executable')
            env.update(BROWSER='/usr/bin/false', NO_COLOR='1')
            with self.lock:
                if job['receipt']['status'] not in ACTIVE:
                    return
                process = subprocess.Popen([sys.executable, '-B', str(Path(__file__).resolve()),
                    '--supervise', str(self.deadline), binary, 'auth', 'login', '--claudeai',
                    '--email', job['receipt']['email']], env=env, stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True,
                    pass_fds=(job['lease'].fileno(),))
                job['process'] = process
            end = time.monotonic() + self.deadline
            buffer = ''
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while process.poll() is None:
                    if time.monotonic() >= end:
                        with self.lock:
                            if job['receipt']['status'] in ACTIVE:
                                self._finish(job, 'error', 'Claude sign-in timed out. Start a new sign-in request.')
                            self._stop(job)
                        break
                    for key, _ in selector.select(.2):
                        chunk = os.read(key.fd, 8192)
                        if not chunk:
                            continue
                        buffer = (buffer + chunk.decode('utf-8', errors='replace'))[-32768:]
                        for match in re.finditer(r'https://[^\s\x1b]+(?=\s|\x1b)', buffer):
                            url = match.group(0)
                            if verification_url(url):
                                with self.lock:
                                    if job['receipt']['status'] in ACTIVE:
                                        job['receipt'].update(status='pending', verificationUrl=url)
                                break
            process.wait(timeout=3)
            with self.lock:
                if job['receipt']['status'] not in ACTIVE:
                    return
            metadata = codex_claude.auth_metadata(job['profile'], force=True)
            with self.runtime.accounts.lock:
                current = self.runtime.accounts._row(job['receipt']['accountKey'])
                if (current.get('claudeOptions') != job['profile'].get('claudeOptions')
                        or current.get('accountId') != job['profile']['accountId']
                        or current.get('disconnected')):
                    raise ValueError('Account settings changed during sign-in')
            refreshed = self.runtime.accounts.refresh(job['receipt']['accountKey'])
            with self.lock:
                if job['receipt']['status'] not in ACTIVE:
                    return
                if (process.returncode == 0 and metadata.get('status') == 'ready'
                        and metadata.get('accountId') == job['profile']['accountId']
                        and refreshed.get('status') == 'ready'
                        and refreshed.get('accountId') == job['profile']['accountId']):
                    self._finish(job, 'ready')
                elif metadata.get('status') == 'ready' and metadata.get('accountId') != job['profile']['accountId']:
                    self._finish(job, 'error', 'A different Claude account signed in. Sign in with ' + job['receipt']['email'] + '.')
                else:
                    self._finish(job, 'error', 'Claude sign-in failed. Start a new sign-in request.')
        except Exception:
            with self.lock:
                if job['receipt']['status'] in ACTIVE:
                    self._finish(job, 'error', 'Cannot complete Claude sign-in. Start a new sign-in request.')
        finally:
            if process:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                process.stdin.close()
                process.stdout.close()
            job['lease'].close()


def supervise():
    """Bound native login lifetime, including an unexpected backend exit."""
    signal.signal(signal.SIGTERM, lambda *_: os.killpg(os.getpgrp(), signal.SIGKILL))
    parent = os.getppid()
    end = time.monotonic() + float(sys.argv[2])
    process = subprocess.Popen(sys.argv[3:])
    while process.poll() is None:
        if os.getppid() != parent or time.monotonic() >= end:
            os.killpg(os.getpgrp(), signal.SIGKILL)
        time.sleep(.2)
    return process.returncode


if __name__ == '__main__' and sys.argv[1:2] == ['--supervise']:
    sys.exit(supervise())
