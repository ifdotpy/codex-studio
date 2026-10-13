# Runtime priority OS boundary

This leaf owns only the parent-side Linux process-group I/O priority syscall,
which is unavailable in the existing safe dependencies. It sets best-effort
class at its lowest level. Callers must pass an owned child's positive group
identity. It never spawns, reads configuration, or changes backend priority.

The safe policy and callers live in `runtime-process::child_priority`. This is
the only runtime crate permitting unsafe code, limited to this one function;
all existing crates retain the workspace ban. Its only dependency is `libc`.
macOS uses safe rustix nice adjustment only; its disk priority is not changed.

Check with `cargo clippy -p studio-runtime-priority-sys -- -D warnings`.
See [the design note](../../../../docs/plans/rust-child-priority.md) for platform
coverage and child-reporting tests.
