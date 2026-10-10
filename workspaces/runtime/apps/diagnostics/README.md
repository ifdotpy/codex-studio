# Diagnostics CLI

## Change Contract

- **Responsibility:** provide the `codex-diagnostics` read-only CLI for the local Studio diagnostics endpoint.
- **Public interface:** `codex-diagnostics [--port PORT]`; JSON goes to stdout, failures go to stderr.
- **Boundaries:** this binary performs one HTTP/1.1 GET to loopback. It has no async runtime, TLS, database, or backend dependency. `serde_json` is the only third-party crate, used to parse JSON while preserving arbitrary-size integers; its `arbitrary_precision` feature supports Python-compatible integer output.
- **Invariants:** the endpoint path and default port are code-owned; successful output matches `json.dumps(..., indent=2, sort_keys=True)` byte for byte, including ASCII escaping and a trailing newline. Requests have a 30-second connect/read/write timeout and do not follow redirects.
- **Configuration owner:** `--port` is the only CLI option; the timeout and endpoint are owned by this crate.
- **Focused check:** `cargo test -p studio-diagnostics tests::formatter_matches_python_json_dump_golden -- --exact`.

## Run and verify

```sh
cargo build --locked --release -p studio-diagnostics
cargo test -p studio-diagnostics
cargo test -p studio-diagnostics tests::formatter_matches_python_json_dump_golden -- --exact
cargo run --locked -p studio-diagnostics -- --help
```

The integration test serves a loopback fixture, checks a committed Python-generated golden output, and compares with Python's `urllib`/`json.dumps` when Python is present. Without Python, it emits a named skip for that comparison and still checks the golden oracle. It never contacts the default live backend.

## Compatibility differences

- Rust accepts port values from 0 through 65535. Python `argparse` accepts any integer, then `urllib` rejects values outside the valid TCP port range during the request. Rust also rejects Python `int()` spellings with surrounding whitespace or underscores.
- Rust contacts loopback directly and does not consult proxy environment variables or follow redirects. The Python `urllib` client may use configured proxies and follows standard redirects.
- Argument parse errors and request/JSON errors keep the same exit status categories (2 for CLI argument errors, 1 for request or response errors) but use concise Rust stderr instead of Python traceback frames. OS wording can vary by platform.
- Rust reports malformed JSON with `serde_json`'s location text. Python reports `JSONDecodeError` traceback details.
- The Rust parser follows standard JSON and rejects non-standard `NaN`/`Infinity` tokens that Python's default decoder accepts. The diagnostics API emits standard JSON.
- `--help` content and status match the current command closely; whitespace and argparse-version-specific formatting may differ.

## Installation and packaging

`install-cli.py` links a prebuilt binary when available, otherwise it runs the locked release build. If neither Cargo nor a prebuilt binary is available, it fails with an actionable error. `scripts/codex-diagnostics` is the checkout compatibility launcher: it executes the release binary or builds it, and fails clearly when Cargo is unavailable. Desktop packaging builds and includes the binary at `Resources/workspace/scripts/codex-diagnostics`.
