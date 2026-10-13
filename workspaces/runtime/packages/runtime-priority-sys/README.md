# Runtime priority OS boundary

This leaf owns only the two parent-side scalar OS calls that are unavailable in
the existing safe dependencies: Linux process-group I/O priority and Darwin
process background policy. Callers must pass an owned child's positive identity.
It never spawns, reads configuration, or changes the backend's priority.

The safe policy and callers live in `runtime-process::child_priority`. This is
the only runtime crate permitting unsafe code, limited to these two functions;
all existing crates retain the workspace ban. Its only dependency is `libc`.

Check with `cargo clippy -p studio-runtime-priority-sys -- -D warnings`.
See [the design note](../../../../docs/plans/rust-child-priority.md) for platform
coverage and the child-reporting tests.
