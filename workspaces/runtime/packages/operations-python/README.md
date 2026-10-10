# studio-operations-python

## Change Contract

- **Responsibility:** Minimal in-process Python binding for `studio-operations`.
- **Public interface:** `studio_operations_native.transition(state_json,
event_json) -> decision_json` and integer `PROTOCOL_VERSION`.
- **Boundaries:** The binding only deserializes, calls the Rust transition, and
  serializes. It performs no lifecycle logic, I/O, subprocess work, or fallback.
- **Invariants:** Malformed inputs and Rust panics become Python exceptions;
  backend import fails clearly if the module or protocol version is unavailable.
- **Configuration owner:** The root Cargo workspace owns PyO3 and native build
  configuration. `install-cli.py` copies an already built platform library to
  the managed Python environment; it never builds implicitly.
- **Focused check:** `cargo build --release -p studio-operations-python`; Python
  contract replay runs in the `server` suite after `install-cli.py` installs it.

The binding uses serde JSON strings to pass the core's own Rust types through
PyO3. That keeps Python conversion code small and rejects malformed typed input
at the boundary. The Rust crate is built with PyO3 0.29, `abi3-py311`, and
`extension-module`; no maturin or second schema is used.

The workspace lint is `unsafe_code = "deny"` because Rust's `forbid` level
cannot be narrowed for macro expansions; the PyO3 `#[pymodule]` entry point has
the only local allow. Binding code contains no handwritten unsafe blocks.
