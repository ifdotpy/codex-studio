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

## Syntax guards and ratchet

The ast-grep rules under `ast-grep/rules` detect direct JSON round trips,
selected JSON-indexed dispatch and serde defaults, parse casts, casts through
unknown, and casts to any. Rules contain the corrective message. They operate
on syntax alone: they do not resolve types, aliases, or prove that other casts,
JSON access, or switch statements are safe. Type-aware checks own those cases.

`ratchet-baseline.json` records each rule's existing matches by repository path
and a SHA-256 fingerprint of whitespace-normalized matched text plus its
occurrence index within that file. `pnpm run check:ast-grep` fails on new matches
and on stale baseline entries. After fixing violations, run
`node workspaces/tooling/apps/repository-checks/ast-grep.ts --shrink` to remove
only stale entries; it cannot add new matches. Review the resulting diff. The
scan runs in `lint:all` and the short tooling tier. It is not in the staged
hook: the full baseline must be checked against a consistent repository snapshot,
while the hook checks index content only.
The root [Cargo lint policy](../../../../Cargo.toml) also rejects boolean-heavy
function signatures, boolean matches, oversized enum variants, and references
to small Copy values. Its native witnesses verify each rule; callbacks required
by external APIs may document a narrow exception.

## Deferred Clippy lints

`just clippy-runtime` runs the workspace Clippy gate and checks deferred lint
findings against the shared ratchet baseline. The Value conversion restrictions
apply to runtime services and store; generated HTTP types are allowed at their
module because the schema determines their shape. After fixing findings, run
`node workspaces/tooling/apps/repository-checks/clippy-ratchet.ts --shrink` and
review the baseline diff. This command removes fixed entries but cannot add new
ones. Fingerprints use the lint and whitespace-normalized diagnostic span, with
an occurrence index per file; changing a span is treated as a new finding.
