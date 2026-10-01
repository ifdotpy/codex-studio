"""Guard and measure SQLite connection scopes used by the backend."""

import logging
import math
import os
import sqlite3
import threading
import time
from collections import defaultdict
from contextlib import contextmanager

_LOCK = threading.Lock()
_COUNTERS = defaultdict(lambda: {"transactions": 0, "longTransactions": 0,
                                 "writeWaits": 0, "writeWaitMs": 0.0,
                                 "lockErrors": 0, "longestTransactionMs": 0.0,
                                 "sites": {}, "_durationSamplesMs": []})
_REPORTED = set()
_LONG_TRANSACTION_MS = 100.0
_WRITE_WAIT_MS = 1.0


def _record(kind, site, elapsed_ms=0.0):
    with _LOCK:
        item = _COUNTERS[kind]
        site_item = item["sites"].setdefault(site, {"transactions": 0,
            "longTransactions": 0, "writeWaits": 0, "writeWaitMs": 0.0,
            "lockErrors": 0, "longestTransactionMs": 0.0,
            "_durationSamplesMs": []})
        if kind == "transaction":
            for target in (item, site_item):
                target["transactions"] += 1
                target["longestTransactionMs"] = max(target["longestTransactionMs"], elapsed_ms)
                if elapsed_ms >= _LONG_TRANSACTION_MS:
                    target["longTransactions"] += 1
                target["_durationSamplesMs"].append(elapsed_ms)
                if len(target["_durationSamplesMs"]) > 256:
                    del target["_durationSamplesMs"][:-256]
        elif kind == "writeWait":
            for target in (item, site_item):
                target["writeWaits"] += 1
                target["writeWaitMs"] += elapsed_ms
        elif kind == "lockError":
            item["lockErrors"] += 1
            site_item["lockErrors"] += 1
    if os.environ.get("CODEX_SQLITE_TRACE") == "1":
        logging.getLogger("codex.sqlite").warning(
            "sqlite %s site=%s durationMs=%.2f", kind, site, elapsed_ms)


def _site(site):
    return site or "sqlite.connect"


def _current_site(db):
    return getattr(db, "_codex_scope_site", getattr(db, "_codex_site", "sqlite.connect"))


def connect(database, *, site=None, **options):
    """Open an instrumented sqlite connection; `site` identifies its owner."""
    options["factory"] = InstrumentedConnection
    db = sqlite3.connect(database, **options)
    db._codex_site = _site(site)
    return db


class InstrumentedConnection(sqlite3.Connection):
    """Connection that measures write waits and transaction lifetimes."""

    def __exit__(self, exc_type, exc_value, traceback):
        try:
            return super().__exit__(exc_type, exc_value, traceback)
        finally:
            if not self.in_transaction:
                self._finish_transaction()

    def execute(self, sql, parameters=(), /):
        started = time.monotonic()
        was_in_transaction = self.in_transaction
        statement = str(sql)
        words = statement.lstrip().split(None, 1)
        is_write = bool(words and words[0].upper() in {
            "INSERT", "UPDATE", "DELETE", "REPLACE", "CREATE", "DROP", "ALTER", "BEGIN"})
        try:
            return super().execute(sql, parameters)
        except sqlite3.OperationalError as error:
            if "locked" in str(error).lower() or "busy" in str(error).lower():
                _record("lockError", _current_site(self))
            raise
        finally:
            elapsed_ms = (time.monotonic() - started) * 1000
            site = _current_site(self)
            if is_write and elapsed_ms >= _WRITE_WAIT_MS:
                _record("writeWait", site, elapsed_ms)
            if not was_in_transaction and self.in_transaction:
                self._codex_transaction_started = time.monotonic()
                self._codex_transaction_site = site
            if was_in_transaction and not self.in_transaction:
                self._finish_transaction()

    def executemany(self, sql, seq_of_parameters, /):
        started = time.monotonic()
        was_in_transaction = self.in_transaction
        statement = str(sql)
        words = statement.lstrip().split(None, 1)
        is_write = bool(words and words[0].upper() in {
            "INSERT", "UPDATE", "DELETE", "REPLACE", "CREATE", "DROP", "ALTER", "BEGIN"})
        try:
            return super().executemany(sql, seq_of_parameters)
        except sqlite3.OperationalError as error:
            if "locked" in str(error).lower() or "busy" in str(error).lower():
                _record("lockError", _current_site(self))
            raise
        finally:
            elapsed_ms = (time.monotonic() - started) * 1000
            site = _current_site(self)
            if is_write and elapsed_ms >= _WRITE_WAIT_MS:
                _record("writeWait", site, elapsed_ms)
            if not was_in_transaction and self.in_transaction:
                self._codex_transaction_started = time.monotonic()
                self._codex_transaction_site = site
            if was_in_transaction and not self.in_transaction:
                self._finish_transaction()

    def _finish_transaction(self):
        started = getattr(self, "_codex_transaction_started", None)
        if started is not None:
            _record("transaction", getattr(self, "_codex_transaction_site", _current_site(self)),
                    (time.monotonic() - started) * 1000)
            self._codex_transaction_started = None
            self._codex_transaction_site = None

    def commit(self):
        try:
            return super().commit()
        finally:
            if not self.in_transaction:
                self._finish_transaction()

    def rollback(self):
        try:
            return super().rollback()
        finally:
            if not self.in_transaction:
                self._finish_transaction()

    def close(self):
        if self.in_transaction:
            _recover(self, _current_site(self), strict=False)
        return super().close()

    def __del__(self):
        # Thread-local reusable connections can outlive their Runtime instance.
        # Close them explicitly so Python does not release an open transaction.
        try:
            self.close()
        except Exception:
            pass


def _recover(db, site, *, strict):
    with _LOCK:
        first = site not in _REPORTED
        _REPORTED.add(site)
    if first:
        logging.getLogger("codex.sqlite").error(
            "SQLite connection left in transaction at %s; rolling back", site)
    with _LOCK:
        _COUNTERS["leaks"].setdefault("count", 0)
        _COUNTERS["leaks"]["count"] += 1
        sites = _COUNTERS["leaks"].setdefault("sites", {})
        sites[site] = sites.get(site, 0) + 1
    try:
        db.rollback()
    except sqlite3.Error:
        logging.getLogger("codex.sqlite").exception("SQLite rollback failed at %s", site)
    if strict:
        raise RuntimeError("SQLite connection left in transaction at " + site)


def assert_clean(db, site, *, strict=None):
    """Recover a leaked transaction before a connection is reused or released."""
    strict = (os.environ.get("CODEX_SQLITE_STRICT") == "1") if strict is None else strict
    if db.in_transaction:
        _recover(db, _site(site), strict=strict)


@contextmanager
def scope(db, site, *, strict=None):
    """Guard a connection at entry/release and apply normal commit/rollback semantics."""
    strict = (os.environ.get("CODEX_SQLITE_STRICT") == "1") if strict is None else strict
    site = _site(site)
    assert_clean(db, site, strict=strict)
    tracked = isinstance(db, InstrumentedConnection)
    previous_site = getattr(db, "_codex_scope_site", None) if tracked else None
    if tracked:
        db._codex_scope_site = site
    try:
        with db:
            yield db
    finally:
        try:
            assert_clean(db, site, strict=strict)
        finally:
            if tracked:
                db._codex_scope_site = previous_site


def diagnostics():
    """Return a bounded, copy-only view of SQLite contention counters."""
    with _LOCK:
        def summary(item):
            values = sorted(item.get("_durationSamplesMs", ()))
            public = {key: value for key, value in item.items()
                      if key not in ("sites", "_durationSamplesMs")}
            if values:
                public["p50TransactionMs"] = round(values[math.ceil(.50 * len(values)) - 1], 3)
                public["p95TransactionMs"] = round(values[math.ceil(.95 * len(values)) - 1], 3)
            return public

        result = {}
        for kind, value in _COUNTERS.items():
            result[kind] = summary(value)
            result[kind]["sites"] = {
                site: summary(item) if isinstance(item, dict) else item
                for site, item in sorted(value["sites"].items())}
        return result
