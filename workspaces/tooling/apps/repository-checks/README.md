# Repository checks

## Change Contract

This app owns checks for staged repository content and the checker invoked by
the pre-commit hook. Its public interface is the checker command used by
`.githooks/`; it may inspect repository files but application code must not
depend on it at runtime. Preserve Git-index-only staged checks and never rewrite
the index or working tree. Configuration and commands are owned by the root
manifest and Oxlint/Oxfmt configuration. Focused check:
`pnpm run test:pre-commit` from the repository root, as defined by the
[root manifest](../../../../package.json).

The compiler and Rust lint policy tests also run negative contract witnesses.
They check that the root Oxlint policy rejects type erasure and unchecked
compiler suppressions, and that native Rust/Clippy rejects unhandled additions
and wildcard patterns that would hide them. The Rust witness copies the owning
root manifest and Clippy settings into a disposable dependency-free crate; it
does not build the application or interpret Rust source itself. Run the focused
witnesses with
`node --test workspaces/tooling/apps/repository-checks/tests/compiler-policy.test.ts workspaces/tooling/apps/repository-checks/tests/rust-lint-policy.test.ts`.
