"""Reproduce code-mode cell retention with a private host and a pure delegate."""
import argparse, hashlib, json, os, pathlib, select, struct, subprocess, tempfile, time
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--host', required=True, type=pathlib.Path)
HOST = parser.parse_args().host.resolve(strict=True)
EXPECTED = '679eedaea70529aa1cffc9bc0a0788c186412663544fa76c09d63b57f383a65a'
assert hashlib.sha256(HOST.read_bytes()).hexdigest() == EXPECTED
started = time.monotonic()
deadline = started + 60
with tempfile.TemporaryDirectory(prefix='studio-cell-proof-') as private:
    env = {k: v for k, v in os.environ.items() if k in ('PATH', 'TMPDIR', 'LANG')}
    env['RUST_LOG'] = 'error'
    p = subprocess.Popen([str(HOST), '--listen', 'stdio'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=private)
    buf = bytearray()
    inbox = []
    closed = set()
    rid = 0

    def send(m):
        b = json.dumps(m, separators=(',', ':')).encode()
        p.stdin.write(struct.pack('<I', len(b)) + b)
        p.stdin.flush()

    def read():
        global buf
        until = min(deadline, time.monotonic() + 4)
        while True:
            if len(buf) >= 4:
                n = struct.unpack('<I', buf[:4])[0]
                assert n < 1024 * 1024, n
                if len(buf) >= n + 4:
                    b = bytes(buf[4:n + 4])
                    del buf[:n + 4]
                    m = json.loads(b)
                    if m['type'] == 'cell/closed':
                        closed.add((m['sessionId'], m['cellId']))
                    return m
            left = until - time.monotonic()
            assert left > 0, 'IPC timeout'
            ready, _, _ = select.select([p.stdout], [], [], left)
            assert ready, 'IPC timeout'
            b = os.read(p.stdout.fileno(), 65536)
            assert b, ('Host exit', p.poll(), p.stderr.read(800))
            buf.extend(b)

    def get(pred):
        for i, m in enumerate(inbox):
            if pred(m):
                return inbox.pop(i)
        while True:
            m = read()
            if pred(m):
                return m
            inbox.append(m)

    def req(session, method, **kw):
        global rid
        rid += 1
        send({'type': 'operation/request', 'id': rid, 'request': {'method': method, 'sessionId': session, **kw}})
        return rid

    def response(i):
        return get(lambda m: m['type'] == 'operation/response' and m['id'] == i)['result']

    def execute(session, i, finite=True):
        tool = {'name': 'echo', 'tool_name': {'name': 'echo', 'namespace': None}, 'description': 'private pure echo', 'kind': 'function', 'input_schema': None, 'output_schema': None}
        r = req(session, 'session/execute', request={'tool_call_id': 'private-' + str(i), 'enabled_tools': [tool] if finite else [], 'source': 'text(await tools.echo({n:' + str(i) + '}));' if finite else 'text("fresh");', 'yield_time_ms': 0 if finite else 1000, 'max_output_tokens': 30})
        v = response(r)
        assert v['status'] == 'ok', v
        cell = v['value']['cellId']
        initial = get(lambda m: m['type'] == 'execute/initialResponse' and m['id'] == r)['result']
        assert initial['status'] == 'ok', initial
        if finite:
            assert 'Yielded' in initial['value'], initial
            d = get(lambda m: m['type'] == 'delegate/request' and m.get('sessionId') == session and (m['request'].get('invocation', {}).get('cell_id') == cell))
            send({'type': 'delegate/response', 'id': d['id'], 'result': {'status': 'ok', 'value': {'type': 'tool/result', 'result': 'finished-' + str(i)}}})
        else:
            assert 'Result' in initial['value'] and initial['value']['Result']['error_text'] is None, initial
        return cell
    try:
        send({'type': 'connection/hello', 'supportedVersions': [1], 'requiredCapabilities': [], 'optionalCapabilities': []})
        hello = read()
        assert hello['type'] == 'connection/ready', hello
        for s in ['private-A', 'private-B']:
            assert response(req(s, 'session/open'))['status'] == 'ok'
        one = execute('private-A', -1)
        v = response(req('private-A', 'session/wait', request={'cell_id': one, 'yield_time_ms': 1000}))
        assert v['status'] == 'ok', v
        assert 'Result' in v['value']['outcome']['LiveCell'], v
        get(lambda m: m['type'] == 'cell/closed' and m['sessionId'] == 'private-A' and (m['cellId'] == one))
        cells = []
        for i in range(128):
            cells.append(('private-A' if i % 2 == 0 else 'private-B', execute('private-A' if i % 2 == 0 else 'private-B', i)))
        time.sleep(0.2)
        v = response(req('private-B', 'session/execute', request={'tool_call_id': 'blocked', 'enabled_tools': [], 'source': 'text("must not run");', 'yield_time_ms': 1000, 'max_output_tokens': 30}))
        assert v == {'status': 'error', 'message': 'code-mode host has too many active cells'}, v
        assert not any((c in closed for c in cells)), ('unconsumed cells unexpectedly closed', len(closed))
        print(json.dumps({'gate': 'shared-128-completed-unpolled', 'accepted': 128, 'sessions': 2, 'rejected129': v['message'], 'closedUnpolled': 0}), flush=True)
        s, c = cells[0]
        v = response(req(s, 'session/wait', request={'cell_id': c, 'yield_time_ms': 1000}))
        assert v['status'] == 'ok' and 'Result' in v['value']['outcome']['LiveCell'], v
        get(lambda m: m['type'] == 'cell/closed' and m['sessionId'] == s and (m['cellId'] == c))
        execute('private-B', 1000, False)
        print(json.dumps({'gate': 'wait-completed-releases-slot', 'result': 'Result', 'freshExec': 'success'}), flush=True)
        results = 1
        for s, c in cells[1:]:
            v = response(req(s, 'session/wait', request={'cell_id': c, 'yield_time_ms': 1000}))
            assert v['status'] == 'ok' and 'Result' in v['value']['outcome']['LiveCell'], v
            assert v['value']['outcome']['LiveCell']['Result']['error_text'] is None, v
            get(lambda m: m['type'] == 'cell/closed' and m['sessionId'] == s and (m['cellId'] == c))
            results += 1
        print(json.dumps({'gate': 'all-finite-cells-completed', 'ResultCount': results}), flush=True)
        r = req('private-A', 'session/execute', request={'tool_call_id': 'unfinished', 'enabled_tools': [], 'source': 'await new Promise(() => {});', 'yield_time_ms': 0, 'max_output_tokens': 30})
        v = response(r)
        assert v['status'] == 'ok'
        c = v['value']['cellId']
        initial = get(lambda m: m['type'] == 'execute/initialResponse' and m['id'] == r)['result']
        assert 'Yielded' in initial['value'], initial
        v = response(req('private-A', 'session/terminate', cellId=c))
        assert v['status'] == 'ok' and 'Terminated' in v['value']['outcome']['LiveCell'], v
        get(lambda m: m['type'] == 'cell/closed' and m['sessionId'] == 'private-A' and (m['cellId'] == c))
        execute('private-B', 1001, False)
        print(json.dumps({'gate': 'unfinished-terminate-releases-slot', 'result': 'Terminated', 'freshExec': 'success'}), flush=True)
        r = req('private-A', 'session/execute', request={'tool_call_id': 'unfinished-shutdown', 'enabled_tools': [], 'source': 'await new Promise(() => {});', 'yield_time_ms': 0, 'max_output_tokens': 30})
        v = response(r)
        assert v['status'] == 'ok'
        c = v['value']['cellId']
        initial = get(lambda m: m['type'] == 'execute/initialResponse' and m['id'] == r)['result']
        assert 'Yielded' in initial['value'], initial
        v = response(req('private-A', 'session/shutdown'))
        assert v['status'] == 'ok' and v['value']['type'] == 'session/closed', v
        execute('private-B', 1002, False)
        assert response(req('private-B', 'session/shutdown'))['status'] == 'ok'
        print(json.dumps({'gate': 'session-shutdown-releases-slots', 'otherSessionFreshExec': 'success', 'seconds': round(time.monotonic() - started, 3), 'hostSHA': EXPECTED}), flush=True)
    finally:
        p.stdin.close()
        try:
            p.wait(timeout=7)
        except subprocess.TimeoutExpired:
            p.terminate()
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait(timeout=3)
        stderr = p.stderr.read(1200).decode(errors='replace')
        if stderr:
            print(json.dumps({'privateHostStderr': stderr}))
