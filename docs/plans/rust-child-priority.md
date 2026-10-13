# Agent child scheduling

The safe policy lives in
[`runtime-process::child_priority`](../../workspaces/runtime/packages/runtime-process/src/child_priority.rs).
`ChildPriority::{Normal, Background}` is read once at Rust server startup from
`CODEX_AGENT_CHILD_PRIORITY=normal|background` (default `background`). Restart
the Rust backend to change it. Normal leaves inherited CPU and disk scheduling
unchanged; malformed settings record a diagnostic and retain Background. The Python
production runtime is unchanged.

Call `ChildPriority::from_environment().apply_before_spawn(&mut command)` on
the final `process_wrap::std::CommandWrap` before spawning. On Unix the command
must already use `ProcessGroup::leader()`. Its parent-side `post_spawn` hook
calls `apply_after_spawn(&Child)` before returning the child to its consumer.
There is no pre-exec callback, launcher executable, backend priority change,
spawn retry, suspension, or job object. The spawn hook performs no diagnostic
I/O (including tracing) and never waits for a diagnostic consumer. Rejections
increment an atomic counter; only its first winner publishes a fixed-size
`PriorityFailure` in a `OnceLock`. `priority_diagnostics()` returns the count
and first error for a health/diagnostics consumer. Its nonblocking snapshot
can briefly see a positive count before the first error is published. The
Rust diagnostics route is not present on this base; its future adapter can
read this function. No unbounded queue, strings, or error history is retained.

| Target  | CPU                                                           | Disk                                | Evidence on this Linux host                                   |
| ------- | ------------------------------------------------------------- | ----------------------------------- | ------------------------------------------------------------- |
| Linux   | `rustix::process::setpriority_pgrp`, nice +5                  | Best-effort class 2, lowest level 7 | Child/group tests; scheduler effectiveness is not measured    |
| macOS   | Same group nice +5                                            | Not set                             | `aarch64-apple-darwin` compile check only; effect unverified  |
| Windows | std `CommandExt::creation_flags(BELOW_NORMAL_PRIORITY_CLASS)` | Not set                             | `x86_64-pc-windows-gnu` compile check only; effect unverified |

Linux group calls include descendants already visible in that group;
descendants forked afterward inherit scheduling. There is a window between
spawn and the parent calls: children can do work before lowering, and a
concurrent fork can keep its parent's old I/O context even without leaving
the group. This is source-derived: Linux copies the
[I/O context](https://github.com/torvalds/linux/blob/v6.12/block/blk-ioc.c#L279)
[before publishing the child](https://github.com/torvalds/linux/blob/v6.12/kernel/fork.c#L2239),
so the [group walk](https://github.com/torvalds/linux/blob/v6.12/block/ioprio.c#L87)
can miss it. A descendant that changes its group or calls `setsid` before
the calls can also escape lowering. This window has no time bound under
contention. Pipe-handshake tests cover forks completed before application
or begun afterward, not this concurrent-kernel-fork interleaving.

macOS's foreign-PID `PRIO_DARWIN_PROCESS` API offers full background policy,
not utility, in [`do_background_proc`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_resource.c).
That policy is too strong for mild deprioritization; it is deliberately not
used. Only safe group nice +5 is applied, with disk priority unchanged.

Windows children inherit a parent's below-normal or idle class unless they
request another class, as documented for
[`CreateProcessW`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw).
The enabled process-wrap features are `std` and Unix-only `process-group`;
none replaces creation flags on Windows. The current callers set no other
creation flags. A future caller needing additional flags must combine them
with this policy explicitly; the std API has no flag getter. No job object is installed: process-wrap can terminate a child on job assignment
failure, which conflicts with advisory priority. These settings are scheduling
hints, not containment; children can deliberately change their own policy.

## Dependencies and the OS boundary

Facts checked against published manifests/source on 2026-10-13:

| Crate                                                            | License and maintenance                                                   | Use and dependency cost                                                     |
| ---------------------------------------------------------------- | ------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| [rustix 1.1.5](https://docs.rs/crate/rustix/1.1.5)               | MIT / Apache-2.0 / LLVM exception; Bytecode Alliance, released 2026-09-16 | Existing safe Unix process APIs; no new transitive crate                    |
| [process-wrap 10.0.1](https://docs.rs/crate/process-wrap/10.0.1) | MIT / Apache-2.0; maintained Watchexec project, pinned in workspace       | Existing groups and parent hooks; no new dependency or feature              |
| [libc 0.2.190](https://docs.rs/crate/libc/0.2.190)               | MIT / Apache-2.0; maintained rust-lang bindings                           | Already locked; sole dependency of the new OS leaf; no new transitive crate |

[`runtime-priority-sys`](../../workspaces/runtime/packages/runtime-priority-sys/src/lib.rs)
is the single runtime crate allowing unsafe code. The owner approved exactly
one scalar-only parent-side call there: Linux group I/O priority. Its lint is
`deny`, with one function-level exception and safety comment. All existing crates keep `unsafe_code = forbid`.
The function rejects nonpositive IDs; the safe adapter additionally verifies
the child leads its own group while its unreaped handle prevents PID reuse.

Alternatives inspected: [ioprio 0.2.0](https://docs.rs/crate/ioprio/0.2.0)
(MIT, 2021-06-08) supports process groups but pulls old `nix ^0.21` plus libc;
its thin syscall implementation informed the Linux boundary. Its old scheduler
documentation is not used. [qos-threads 0.1.3](https://docs.rs/crate/qos-threads/0.1.3)
(MIT/Apache-2.0, 2026-08-14, libc only) affects the calling thread/process and
does not provide foreign-PID background policy.
[win32job 2.0.3](https://docs.rs/crate/win32job/2.0.3)
(MIT/Apache-2.0, 2025-05-15, windows and thiserror) has no safe low-I/O method;
windows-sys process-information calls require unsafe. Windows disk priority is
therefore explicitly unsupported here. No alternative crate was added.

## Host prerequisites and remaining work

Read-only observation on 2026-10-13: the NVMe queue reports
`[none] mq-deadline kyber bfq`; the user systemd instance delegates
`cpu memory pids`, not `io`. Linux accepts and reports best-effort I/O priority at level 7, but
the `none` scheduler does not arbitrate requests with those classes. An
administrator must select a scheduler supporting I/O priorities (BFQ, or
mq-deadline's class handling) before expecting a disk scheduling benefit.
The kernel documents the interface in
[I/O priorities](https://docs.kernel.org/block/ioprio.html).

For cgroup weights, the host must delegate and enable the `io` controller,
place agent workloads in a separate group from the backend, and set the
agent group's `io.weight` lower than the backend's. The underlying block
controller must support proportional weights; a writable file alone does not
prove an effect. See [cgroup v2 I/O](https://docs.kernel.org/admin-guide/cgroup-v2.html#io).
There is no cgroup discovery, migration, weight write, or scheduler mutation
in this change; the owner explicitly assigned these to host configuration.

On main, `bounded_child` applies Background to agent commands. The Studio-owned
Claude status probe (`account_driver`, eight-second deadline) and model catalog
exchange (`model_catalog`, 30-second deadline) explicitly use Normal: they
preserve inherited scheduling even when the configured agent level is Background.
`provider_stdio` agent sessions, `stream_child`, and the Rust supervisor's
agent children live on other branches: their integrator must add the final
configured `apply_before_spawn` call and preserve an owned Unix group.
Initialize `from_environment()` at supervisor startup as the server does.
They are not wired by this branch. Mild priorities remain advisory; responsiveness
under contention is unmeasured. Restart with `CODEX_AGENT_CHILD_PRIORITY=normal`
to disable agent lowering if provider deadlines are affected; a contention
validation run requires separate authorization.

Priorities do not replace two runtime requirements: agent temporary files and
test state should default to tmpfs, and message intake should enqueue bounded
work to one disk writer instead of blocking on disk itself. An `accepted`
receipt still follows the durable write. This change implements neither of
those independent runtime architecture tasks.

Measured checks after review corrections: 203 runtime-process/runtime-services
tests passed with two existing skips. This includes the full undrained stderr
pipe regression, first-error/count snapshots, mild-priority inheritance, and
both Studio helper callers preserving inherited scheduling. The two priority crates compile for macOS and
Windows; Windows reports existing unused imports/constants in the transport
and supervisor client, outside this change. Workspace Clippy passes on Linux.

Focused checks: `cargo fmt --all -- --check`, workspace Clippy with
`--all-targets -- -D warnings`, and `cargo nextest run -p studio-runtime-process
-p studio-runtime-services --test-threads 4`. Child probes use pipe handshakes,
`ps` and `ionice`; no workload, stress run, or mutation test is involved.
