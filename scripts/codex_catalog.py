"""Account-local model metadata with bounded waits and exact late-response reuse."""
from __future__ import annotations

import concurrent.futures
import copy
import threading
import time


class CatalogPending(RuntimeError):
    """Metadata is pending. The caller has not reached worker creation."""


class CatalogUnavailable(RuntimeError):
    """Metadata failed before the caller could create any worker."""


class ModelCatalogCache:
    """One in-flight read per account/connection, five-minute successful snapshots.

    A timeout leaves the native future attached. The exact late result can fill
    the cache; repeated callers cannot create duplicate pending native reads.
    Expired metadata never admits a model. No runtime or database lock is used.
    """

    def __init__(self, *, ttl=300, wait_seconds=5, clock=time.monotonic):
        self.ttl = ttl
        self.wait_seconds = wait_seconds
        self.clock = clock
        self.lock = threading.Lock()
        self.entries = {}

    def read(self, account, server, connection_id, current, *, submit=None):
        if not current():
            raise CatalogUnavailable("Model catalog connection changed; no workers were created")
        start = False
        with self.lock:
            entry = self.entries.get(account)
            same = (entry is not None and entry["server"] is server
                    and entry["connectionId"] == connection_id)
            if same and entry["expires"] > self.clock():
                result = copy.deepcopy(entry["value"])
                future = None
            else:
                if not same or entry["future"].done():
                    entry = {"server": server, "connectionId": connection_id,
                             "future": concurrent.futures.Future(),
                             "expires": 0, "value": None}
                    self.entries[account] = entry
                    start = True
                future = entry["future"]
        if start:
            rows, seen_cursors, seen_models = [], set(), set()
            first_page = None

            def fail(error):
                if not future.done():
                    future.set_exception(CatalogUnavailable(
                        f"Model catalog unavailable; no workers were created: {error}"))

            def complete(native_future):
                nonlocal first_page
                try:
                    value = native_future.result()
                    if (not isinstance(value, dict) or not isinstance(value.get("data"), list)
                            or any(not isinstance(row, dict)
                                   or not isinstance(row.get("model"), str)
                                   or not row["model"].strip() for row in value["data"])):
                        raise ValueError("Invalid model/list response")
                    if not current():
                        raise RuntimeError("Model catalog connection changed")
                    if first_page is None:
                        first_page = copy.deepcopy(value)
                    for row in value['data']:
                        if row['model'] in seen_models:
                            raise ValueError("Duplicate model in model/list pages")
                        seen_models.add(row['model'])
                        rows.append(copy.deepcopy(row))
                    cursor = value.get('nextCursor')
                    if cursor is not None:
                        if (not isinstance(cursor, str) or not cursor or cursor in seen_cursors
                                or len(seen_cursors) >= 100):
                            raise ValueError("Invalid or repeated model/list cursor")
                        seen_cursors.add(cursor)
                        submit_page(cursor)
                        return
                    value = {**first_page, 'data': rows}
                    if 'nextCursor' in first_page or seen_cursors:
                        value['nextCursor'] = None
                    with self.lock:
                        if self.entries.get(account) is entry:
                            entry.update(value=value, expires=self.clock() + self.ttl)
                    future.set_result(value)
                except Exception as error:
                    fail(error)

            def submit_page(cursor=None):
                try:
                    if not current():
                        raise RuntimeError("Model catalog connection changed")
                    submitted = (submit or server.submit)("model/list", {"limit": 100, **({'cursor': cursor} if cursor is not None else {})})
                except Exception as error:
                    # Retain a submitted read after an unknown pipe-write outcome.
                    submitted = getattr(error, "submitted", None)
                    if submitted is None:
                        fail(error)
                if submitted is not None:
                    native_future = submitted[2] if isinstance(submitted, tuple) else submitted
                    native_future.add_done_callback(complete)

            submit_page()
        if future is not None:
            try:
                result = copy.deepcopy(future.result(self.wait_seconds))
            except concurrent.futures.TimeoutError as error:
                raise CatalogPending(
                    "Model catalog is pending; no workers were created. "
                    "The existing metadata request remains available for recovery") from error
        if not current():
            raise CatalogUnavailable("Model catalog connection changed; no workers were created")
        return result


def runtime_catalog(runtime, account):
    # connect validates account existence and handles an offline connection.
    # Never wait for the catalog under Runtime.lock or Runtime.start_lock.
    server = runtime.connect(account)
    connection_id = runtime.connection_ids.get(account)
    cache = runtime.__dict__.setdefault("_catalog_cache", ModelCatalogCache())
    current = lambda: (
        not runtime.closed and runtime.servers.get(account) is server
        and runtime.connection_current(account, connection_id))
    submit = None
    from codex_runtime import AppServer
    if runtime.factory is AppServer and runtime.accounts.get(account).get("provider", "codex") == "codex":
        from codex_model_catalog_reader import submit_model_catalog
        home = runtime.accounts.home(account)

        def submit(_method, _params):
            return submit_model_catalog(home, isolated=account != "default", current=current)

    return cache.read(account, server, connection_id, current, submit=submit)
