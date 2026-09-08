"""Install the account cost route without replacing the server or native workers."""
import importlib.util
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from codex_efficiency_update import fingerprint


def apply(server, expected_get, expected_close):
    handler = server.RequestHandlerClass
    previous_get = handler.do_GET
    previous_close = server.server_close
    if (fingerprint(previous_get) != expected_get
            or fingerprint(previous_close) != expected_close):
        raise RuntimeError('HTTP methods changed; no account cost update applied')
    closure = dict(zip(previous_get.__code__.co_freevars,
                       [cell.cell_contents for cell in previous_get.__closure__ or ()]))
    canvas = closure.get('canvas')
    lock = closure.get('terminal_lock')
    if canvas is None or canvas.runtime is None or lock is None:
        raise RuntimeError('Unknown HTTP state; no account cost update applied')
    path = Path(__file__).with_name('codex_costs.py')
    spec = importlib.util.spec_from_file_location('studio_account_costs', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    manager = module.AccountCostReader(canvas.root, canvas.runtime.accounts)

    def do_GET(self):
        route = urlparse(self.path)
        if route.path != '/api/costs':
            return previous_get(self)
        if not self.trusted():
            return self.send({'error': 'Local origin required'}, 403)
        try:
            key = parse_qs(route.query).get('account_key', ['default'])[0]
            return self.send(manager.snapshot(key))
        except (ValueError, RuntimeError, OSError) as error:
            return self.send({'error': str(error)}, 400)

    def server_close():
        manager.close()
        previous_close()

    if not lock.acquire(timeout=10):
        raise RuntimeError('HTTP state is busy; no account cost update applied')
    try:
        if (handler.do_GET is not previous_get
                or fingerprint(server.server_close) != expected_close
                or canvas.runtime.closed):
            raise RuntimeError('HTTP state changed; no account cost update applied')
        handler.do_GET = do_GET
        server.server_close = server_close
    finally:
        lock.release()
    return {'status': 'applied', 'get': fingerprint(do_GET),
            'close': fingerprint(server_close)}
