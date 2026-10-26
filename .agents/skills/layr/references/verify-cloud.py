#!/usr/bin/env python3
"""Run the skill's shell recipes against a real CLI and disposable HTTPS/file store.

Usage: python3 verify-cloud.py --layr-source /path/to/layr --root /scratch/new-run
Build the external CLI with cargo build -j 4 first. On this host, root must be
on case-sensitive APFS. The script retains the scratch store and transcript.
Only the explicitly selected verification uses a loopback gateway stand-in.
"""
import argparse
import base64
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import ssl
import subprocess
import threading
import time
import urllib.parse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--layr-source', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--binary', type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    root.mkdir(mode=0o700)
    source = args.layr_source.resolve()
    binary = (args.binary or source / 'target/debug/layr').resolve()
    skill = Path(__file__).resolve().parent.parent / 'SKILL.md'
    recipes = dict(re.findall(r'<!-- recipe: ([\w-]+) -->\s*```sh\n(.*?)\n```', skill.read_text(), re.S))
    transcript = (root / 'session.log').open('w')
    env = {k: v for k, v in os.environ.items() if not k.startswith(('LAYR_', 'AWS_', 'GIT_'))}
    env.update(LAYR_ROOT=str(root / 'store'), LAYR_PERSON_DIR=str(root / 'person'),
               LAYR_PERSON_LOGIN='reviewer@example.com', LAYR_PERSON_KEY_FILE=str(root / 'person.key'),
               LAYR_AUTHOR_NAME='Worker', LAYR_AUTHOR_EMAIL='worker@example.com')
    cwd = root

    def run(command, current_env=None, expected=0, record=True):
        proc = subprocess.run(command, cwd=cwd, env=current_env or env, text=True,
                              capture_output=True, timeout=120)
        if record:
            transcript.write('$ ' + shlex.join(map(str, command)) + '\n' + proc.stdout + proc.stderr + f'[exit {proc.returncode}]\n')
            transcript.flush()
        assert proc.returncode == expected, (command, proc.returncode, proc.stdout, proc.stderr)
        return proc.stdout

    def layr(*command, **kwargs):
        return run([str(binary), *command], **kwargs)

    def data(*command):
        return json.loads(layr(*command))

    def recipe(name):
        command = recipes[name]
        # Every agent command comes directly from the skill. A missing recipe fails.
        assert all(not re.search(r'\bgh\b|\bgit\b', line) for line in command.splitlines())
        transcript.write(f'RECIPE {name}\n')
        transcript.flush()
        output = run(['/bin/sh', '-ec', command])
        print('PASS', name, flush=True)
        return output

    def check(value, label):
        assert value, label
        print('PASS', label, flush=True)
        transcript.write('PASS ' + label + '\n')
        transcript.flush()

    # Provision an isolated identity directly: no paper key enters agent credentials.
    deps = source / 'target/debug/deps'
    libraries = sorted(deps.glob('liblayr_store-*.rlib'), key=lambda p: p.stat().st_mtime)
    helper = root / 'person-fixture'
    run(['rustc', '--edition=2021', str(Path(__file__).with_name('person-fixture.rs')),
         '--extern', 'layr_store=' + str(libraries[-1]), '-L', 'dependency=' + str(deps), '-o', str(helper)])
    run([str(helper), 'person', env['LAYR_PERSON_DIR'], env['LAYR_PERSON_KEY_FILE']])
    person = data('person', 'show', '--json')
    check(not (root / 'person/paper.age').exists(), 'agent identity has no paper slot')
    fixture_env = dict(env)
    fixture_env.pop('LAYR_PERSON_KEY_FILE')
    (root / 'seed').mkdir()
    (root / 'seed/answer.txt').write_text('initial\n')
    layr('init', 'app', '--from', str(root / 'seed'), current_env=fixture_env)
    cwd = root / 'store/projects/app/lines/main'
    store = root / 'objects'
    layr('remote', 'add', 'fixture', store.as_uri(), '--paper-key-out', str(root / 'operator-paper'), current_env=fixture_env)
    (root / 'operator-paper').unlink()
    # Only the fixture operator sees this output path; no key bytes are read or logged.
    layr('person', 'keys', '--store', store.as_uri())
    layr('remote', 'grant', 'fixture', '--person', env['LAYR_PERSON_LOGIN'], '--recipient', person['recipient'],
         '--invite-out', str(root / 'invite.age'))
    layr('person', 'accept', '--invite-file', str(root / 'invite.age'), '--remote', store.as_uri())
    person = data('person', 'show', '--json')
    layr('access', 'person-key', env['LAYR_PERSON_LOGIN'], person['signing_key'])
    layr('access', 'grant', 'person:' + env['LAYR_PERSON_LOGIN'], 'maintainer')
    layr('config', 'check.unit', 'test "$(cat answer.txt)" = fixed')
    layr('protect', 'main', '--check', 'unit', '--approvals', '1', '--approvers', 'person:' + env['LAYR_PERSON_LOGIN'])
    layr('push', 'fixture')

    # TLS fixture transport uses the real encrypted file remote objects.
    run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
         '-keyout', str(root / 'ca.key'), '-out', str(root / 'ca.crt'), '-subj', '/CN=M6 test CA',
         '-addext', 'basicConstraints=critical,CA:TRUE'], record=False)
    run(['openssl', 'req', '-new', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(root / 'tls.key'),
         '-out', str(root / 'tls.csr'), '-subj', '/CN=localhost'], record=False)
    (root / 'tls.ext').write_text('basicConstraints=critical,CA:FALSE\nsubjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n')
    run(['openssl', 'x509', '-req', '-in', str(root / 'tls.csr'), '-CA', str(root / 'ca.crt'),
         '-CAkey', str(root / 'ca.key'), '-CAcreateserial', '-out', str(root / 'tls.crt'), '-days', '1',
         '-extfile', str(root / 'tls.ext')], record=False)
    descriptor = json.loads((store / 'store.json').read_text())
    proof = 'm6-read-proof'
    encrypted = subprocess.run([str(helper), 'encrypt', descriptor['recipient']], input=proof.encode(), capture_output=True, check=True).stdout
    session_challenge = base64.b64encode(secrets.token_bytes(32)).decode()
    session_token = secrets.token_hex(24)
    read_token = secrets.token_hex(24)
    write_token = secrets.token_hex(24)
    (root / 'write.token').write_text(write_token)
    (root / 'write.token').chmod(0o600)
    gateway_events = []
    errors = []

    class Gateway(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def reply(self, status, body):
            if not isinstance(body, bytes):
                body = json.dumps(body).encode()
            self.send_response(status)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            self.handle_request()

        def do_POST(self):
            self.handle_request()

        def do_PUT(self):
            self.handle_request()

        def handle_request(self):
            try:
                self.route()
            except Exception as error:
                errors.append(f'{self.command} {self.path}: {error!r}')
                self.reply(500, {'error': 'fixture failed'})

        def route(self):
            parsed = urllib.parse.urlsplit(self.path)
            path = urllib.parse.unquote(parsed.path)
            body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            gateway_events.append((self.command, path))
            if path.startswith('/p/app/'):
                self.reply(404, {})
                return
            if path == '/api/session/challenge':
                assert urllib.parse.parse_qs(parsed.query)['login'] == [env['LAYR_PERSON_LOGIN']]
                self.reply(200, {'challenge': session_challenge, 'expires': int(time.time()*1000) + 30000})
                return
            if path == '/api/session':
                request = json.loads(body)
                assert request['login'] == env['LAYR_PERSON_LOGIN'] and request['challenge'] == session_challenge
                signature = base64.b64encode(bytes.fromhex(request['signature'])).decode()
                subprocess.run([str(helper), 'verify', request['login'], session_challenge, signature, person['signing_key']], check=True, capture_output=True)
                self.reply(200, {'token': session_token, 'expires': int(time.time()*1000) + 600000, 'login': env['LAYR_PERSON_LOGIN']})
                return
            assert self.headers.get('Authorization') in ['Bearer ' + session_token, 'Bearer ' + read_token, 'Bearer ' + write_token]
            if path == '/api/person':
                self.reply(200, dict(login=env['LAYR_PERSON_LOGIN'], recipient=person['recipient'],
                                     signing_key=person['signing_key'], index=base64.b64encode((root/'person/index.age').read_bytes()).decode()))
                return
            assert path.startswith('/v1/p/app/')
            action = path[len('/v1/p/app/'):]
            if action == 'challenge':
                self.reply(200, {'challenge': base64.b64encode(encrypted).decode()})
            elif action == 'token':
                assert json.loads(body)['challenge'] == proof
                self.reply(200, {'token': read_token, 'expires': int(time.time()*1000) + 300000})
            elif action == 'slots':
                self.reply(200, {'slots': [{'name': p.stem, 'data': base64.b64encode(p.read_bytes()).decode()}
                                         for p in (store/'slots').glob('*.age')]})
            elif action == 'objects':
                prefix = urllib.parse.parse_qs(parsed.query).get('prefix', [''])[0]
                self.reply(200, {'objects': [{'key': str(p.relative_to(store)), 'modified': int(p.stat().st_mtime*1000)}
                                            for p in store.rglob('*') if p.is_file() and str(p.relative_to(store)).startswith(prefix)]})
            else:
                key = action.removeprefix('objects/')
                assert not key.startswith('/') and '..' not in Path(key).parts
                file = store / key
                if self.command == 'PUT':
                    file.parent.mkdir(parents=True, exist_ok=True)
                    try:
                        with file.open('xb') as output:
                            output.write(body)
                        self.reply(201, b'')
                    except FileExistsError:
                        self.reply(412, b'')
                else:
                    self.reply(200, file.read_bytes()) if file.is_file() else self.reply(404, {})

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Gateway)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(root / 'tls.crt', root / 'tls.key')
    server.socket = context.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        bindir = root / 'bin'
        bindir.mkdir()
        (bindir / 'layr').symlink_to(binary)
        env.update(PATH=str(bindir) + os.pathsep + env['PATH'], LAYR_SITE=f'https://localhost:{server.server_port}',
                   LAYR_CLOUD_URL=f'layr+https://localhost:{server.server_port}/app', LAYR_REMOTE='cloud',
                   LAYR_GATEWAY_CA_FILE=str(root / 'ca.crt'), LAYR_MESSAGE='Fix the answer',
                   LAYR_GATEWAY_TOKEN_FILE=str(root / 'write.token'),
                   LAYR_REPLY='Fixed in the amended version.')
        recipe('cloud-identity')
        recipe('cloud-remote')
        layr('line', 'new', 'feature')
        cwd = cwd.parent / 'feature'
        (cwd / 'answer.txt').write_text('broken\n')
        recipe('cloud-open')
        change = next(c['change_id'] for c in data('change', 'list', '--json') if c['message'] == env['LAYR_MESSAGE'])
        env['LAYR_CHANGE'] = change
        layr('try', 'head', '--check', 'unit', expected=1)
        comment = data('comment', change, 'Please fix the answer.', '--path', 'answer.txt', '--line', '1', '--json')
        env['LAYR_COMMENT'] = comment['id']
        layr('push', 'cloud')
        status = recipe('cloud-status')
        status = json.loads(status.splitlines()[-1])
        check(status['versions'][-1]['checks'][0]['exit'] == 1, 'agent sees a failing check in change JSON')
        check(status['comments'][0]['text'] == 'Please fix the answer.', 'agent sees the review comment')
        (cwd / 'answer.txt').write_text('fixed\n')
        recipe('cloud-update')
        show = data('change', 'show', change, '--json')
        check(len(show['versions']) == 2 and show['versions'][0]['checks'][0]['exit'] == 1,
              'amend keeps ChangeId and old failing evidence')
        env['LAYR_STATE'] = show['versions'][-1]['state']['id']
        layr('try', env['LAYR_STATE'], '--check', 'unit')
        recipe('cloud-reply')
        check(data('change', 'show', change, '--json')['comments'][-1]['reply_to'] == comment['id'], 'reply retains its parent ID')
        recipe('cloud-approve')
        request_output = recipe('cloud-person-approve')
        request_id = json.loads(request_output)['id']
        applied = data('requests', 'apply', '--remote', 'cloud', '--json')
        check(next(r for r in applied if r['id'] == request_id)['status'] == 'applied', 'executor applies the person approval')
        layr('push', 'cloud')
        queued = recipe('cloud-queue-add')
        entry = json.loads(queued.splitlines()[0])
        env['LAYR_QUEUE_ENTRY'] = entry['id']
        check(entry['state'] == env['LAYR_STATE'], 'queue pins the reviewed state')
        recipe('cloud-queue-remove')
        check(not data('queue', 'list', '--json'), 'withdrawn entry is absent')
        recipe('cloud-queue-add')
        recipe('cloud-merge')
        entries = data('queue', 'list', '--json')
        check(entries[-1]['status'] == 'done' and entries[-1]['result'], 'queue records a successful merge result')
        check(layr('show', 'main:answer.txt').strip() == 'fixed', 'main contains the fix')
        env['LAYR_STATE'] = entries[-1]['result']
        env['LAYR_RELEASE'] = 'release/m6-test'
        env['LAYR_ARTIFACT'] = str(root / 'artifact.txt')
        Path(env['LAYR_ARTIFACT']).write_text('m6 release fixture\n')
        layr('ref', 'set', env['LAYR_RELEASE'], env['LAYR_STATE'])
        layr('push', 'cloud')
        recipe('cloud-release')
        files = data('release', 'files', env['LAYR_RELEASE'], '--remote', 'cloud', '--json')
        check(files[0]['sha256'] == hashlib.sha256(Path(env['LAYR_ARTIFACT']).read_bytes()).hexdigest(), 'release file SHA-256 matches its source')
        original_root = env['LAYR_ROOT']
        env.update(LAYR_ROOT=str(root / 'observer-store'), LAYR_PROJECT_COPY='observer')
        recipe('cloud-clone')
        env.update(LAYR_PROJECT='observer', LAYR_LINE='main')
        remote = data('change', 'show', change, '--json')
        check(remote['versions'][0]['checks'][0]['exit'] == 1 and remote['versions'][-1]['checks'][0]['exit'] == 0,
              'fresh clone reads both exact-version check outcomes from encrypted remote objects')
        check(remote['comments'][-1]['reply_to'] == comment['id'], 'fresh clone reads the remote discussion thread')
        check(layr('show', 'main:answer.txt').strip() == 'fixed', 'fresh clone reads the merged main files')
        env['LAYR_ROOT'] = original_root
        env.pop('LAYR_PROJECT')
        env.pop('LAYR_LINE')
        check(not errors, 'gateway stand-in completed without handler errors')
        check(any(p == '/api/session' for _, p in gateway_events), 'gateway verifies a real person signature')
        check(any('/objects/records/' in p for m, p in gateway_events if m == 'PUT'), 'push sends signed records through HTTPS')
        check(set(recipes) == {line.split()[1] for line in (root/'session.log').read_text().splitlines() if line.startswith('RECIPE ')}, 'every skill recipe ran')
        (root / 'gateway-events.json').write_text(json.dumps(gateway_events, indent=2))
        print('Transcript:', root / 'session.log', flush=True)
    finally:
        (root / 'gateway-events.json').write_text(json.dumps(gateway_events, indent=2))
        (root / 'gateway-errors.json').write_text(json.dumps(errors, indent=2))
        server.shutdown()
        server.server_close()
        thread.join()
        transcript.close()


if __name__ == '__main__':
    main()
