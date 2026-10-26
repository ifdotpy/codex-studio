# Diagnostics CLI

## Change Contract

- **Responsibility:** provide the `codex-diagnostics` read-only CLI for the local Studio diagnostics endpoint.
- **Public interface:** `codex-diagnostics [--port PORT]`; JSON goes to stdout, failures go to stderr.
- **Boundaries:** this binary performs one HTTP/1.1 GET to loopback. It has no async runtime, TLS, database, or backend dependency. `serde_json` is the only third-party crate and parses and formats standard JSON. Integer values within `i64`/`u64` remain exact; floating-point values use Rust's shortest round-trip spelling.
- **Invariants:** the endpoint path and default port are code-owned; successful output is deterministic, pretty-printed JSON with sorted object keys and a trailing newline. Requests have a 30-second connect/read/write timeout and do not follow redirects.
- **Configuration owner:** `--port` is the only CLI option; the timeout and endpoint are owned by this crate.
- **Focused check:** `cargo nextest run -p studio-diagnostics -E 'test(~fixture_output_matches_rust_golden)'`.

## Run and verify

```sh
cargo build --locked --release -p studio-diagnostics
cargo nextest run -p studio-diagnostics
cargo nextest run -p studio-diagnostics -E 'test(~tests::formatter_matches_python_json_dump_golden)'
cargo run --locked -p studio-diagnostics -- --help
```

The integration test serves a loopback fixture and checks a committed golden output. It never contacts the default live backend.

## Compatibility differences

- Rust accepts port values from 0 through 65535. Python `argparse` accepts any integer, then `urllib` rejects values outside the valid TCP port range during the request. Rust also rejects Python `int()` spellings with surrounding whitespace or underscores.
- Rust contacts loopback directly and does not consult proxy environment variables or follow redirects. The Python `urllib` client may use configured proxies and follows standard redirects.
- CLI argument errors return 2, and request or response errors return 1 in both implementations. Rust emits concise stderr rather than Python traceback frames. For the checked fixtures, both exit 1 on connection refusal, timeout, HTTP 503, and malformed JSON; stderr differs: Rust prints `<urlopen error ...>`, `HTTP Error 503: Service Unavailable`, or `invalid JSON response: ...` after the `codex-diagnostics:` prefix, while Python appends a traceback ending in `URLError`, `TimeoutError`, `HTTPError`, or `JSONDecodeError` respectively. Platform OS wording can vary.
- Rust reports malformed JSON with `serde_json`'s location text. Python reports `JSONDecodeError` traceback details.
- The Rust parser follows standard JSON and rejects non-standard `NaN`/`Infinity` tokens that Python's default decoder accepts. The diagnostics API emits standard JSON.
- **Announced output change:** float text may differ from Python's `json.dumps`; for example, `1e-05` becomes `0.00001` and `1e-07` becomes `1e-7`. Integers within `i64`/`u64` remain exact. Larger integer tokens are printed as floating-point approximations, so their original integer value is not preserved. Floating-point tokens outside finite `f64` range take the invalid-JSON failure path. Non-ASCII text and DEL are emitted directly instead of as `\\u` escapes.
- On the current Python argparse version, `--help` and unknown-flag output, status, and stderr match byte for byte (0 for help, 2 for an unknown flag). Other argparse versions may format help differently.

## Installation and packaging

`install-cli.py` installs the existing thirteen commands without requiring Rust. It additionally links `codex-diagnostics` when a release binary exists in `target/release`, the configured `CARGO_TARGET_DIR/release`, or a packaged `resources/workspace/bin`; otherwise it prints a one-line build instruction and exits successfully. The installer never invokes Cargo. In a development checkout, build with `cargo build --locked --release -p studio-diagnostics`; `cargo clean` removes that binary and a subsequent install will omit the diagnostics link until it is rebuilt. Desktop packaging builds with Cargo and includes the binary at `Resources/workspace/bin/codex-diagnostics`.
