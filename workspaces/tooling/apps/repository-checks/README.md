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
