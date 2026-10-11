# studio-operations-python

## Change Contract

- **Responsibility:** Minimal in-process Python binding for `studio-operations`.
- **Public interface:** `studio_operations_native.transition_record(record_json,
event, ...) -> TransitionResult` and integer `PROTOCOL_VERSION`.
- **Boundaries:** The binding deserializes a typed persisted-record view, adapts
  it to the pure Rust lifecycle core, and returns typed persistence fields. It
  performs no I/O, subprocess work, or fallback.
- **Invariants:** Malformed inputs and Rust panics become Python exceptions;
  backend import fails clearly if the module or protocol version is unavailable.
- **Configuration owner:** The root Cargo workspace owns PyO3 and native build
  configuration. `install-cli.py` copies an already built platform library to
  the managed Python environment; it never builds implicitly.
- **Focused check:** `cargo build --release -p studio-operations-python`; Python
  contract replay runs in the `server` suite after `install-cli.py` installs it.

Python passes the stored request record as JSON while event and proof names use
closed type-stub literals. Rust owns the persisted-record conversion, lifecycle
state, transition, and result-to-storage mapping. The Rust crate is built with
PyO3 0.29, `abi3-py311`, and `extension-module`; no maturin or second runtime
schema is used.

The workspace lint is `unsafe_code = "deny"` because Rust's `forbid` level
cannot be narrowed for macro expansions; the PyO3 `#[pymodule]` entry point has
the only local allow. Binding code contains no handwritten unsafe blocks.
