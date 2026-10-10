"""Run one host-requested command in an existing guest workspace."""
import argparse
import base64
import sys
import uuid


def execute(client, agent_id, cwd, command, *, stdin=b'', request_id=None, output=None, error=None):
    output = output or sys.stdout.buffer
    error = error or sys.stderr.buffer
    identity = request_id or str(uuid.uuid4())
    params = {'argv': command, 'cwd': cwd, 'agentId': agent_id, 'timeoutSeconds': 300}
    if stdin:
        params['stdin'] = base64.b64encode(stdin).decode('ascii')
    result = None
    for frame in client.stream('exec', params, request_id=identity, timeout=310):
        if frame.get('event') == 'output':
            event = frame['data']
            target = output if event['stream'] == 'stdout' else error
            target.write(base64.b64decode(event['data'], validate=True))
            target.flush()
        if 'result' in frame:
            result = frame['result']
    if result is None or result.get('exitCode') is None:
        raise RuntimeError('The guest command has no proven result; request ID: ' + identity)
    return result['exitCode']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--agent', required=True)
    parser.add_argument('--cwd', required=True)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command:
        parser.error('Supply a command')
    from codex_linux_vm import connect
    try:
        return execute(connect(), args.agent, args.cwd, command)
    except Exception as caught:
        print('Linux VM command failed: ' + str(caught), file=sys.stderr)
        return 125


if __name__ == '__main__':
    sys.exit(main())
