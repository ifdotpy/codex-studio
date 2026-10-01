# Native notifications

## Change Contract

This component turns native hook and notice events into Studio transcript items
or account notices. Dispatch enters `runtime.db()` to receive a SQLite
connection and transaction, which it uses to update runtime rows. It does not
configure database storage, connect to Codex, submit requests, or decide whether
a tool is allowed. The runtime owns storage and account connections. Focused
dispatch checks live in `tests/`; cross-component runtime contracts remain in
the repository `tests/` directory.

### How the route works

When a native event includes a thread ID, the dispatcher asks SQLite (the
server's local database) for agents with that thread ID and account. The
database already has an index for those two fields, so a busy server does not
have to open every agent record just to find the recipients. If several agents
share that exact account and thread, each one still receives the event. Deleted
agents are ignored after their rows are decoded.

An account warning can arrive before a chat exists. If the event has no thread
ID and is one of the account notice methods, the component saves it under the
account and connection. Other threadless events keep their existing active
agent routing. A resolved approval request records that native processing has
finished; it does not say the user approved the request.

Hooks update one transcript item for a run. A later completion replaces the
running item; a quiet successful hook writes a tombstone (an empty update) so
clients clear the running item. Hook delivery is still allowed after its turn ends, provided the
connection and thread still match. Turn errors remain owned by
[`codex_native_errors.py`](../codex_native_errors.py).

### Contributor checks

Run the focused package checks from the repository root:

```sh
python3 scripts/native_notifications/tests/test_dispatch.py
python3 scripts/native_notifications/benchmarks/benchmark.py --check
```

For timings at several database sizes:

```sh
python3 scripts/native_notifications/benchmarks/benchmark.py --rows 1000 10000 50000 --rounds 100
```

The benchmark inserts synthetic agent rows into a temporary database, calls
the production `matching_agents` function, and prints p50, p95, p99, query-plan
and decoded-row evidence. It makes no native connection and does not write
results into the checkout. Timings describe local lookup cost; they do not
measure native transport or end-to-end notification delivery.
