import { test } from "node:test";
import { execFileSync } from "node:child_process";
import path from "node:path";

// Node owns the fast-tier summary; the helper suite uses only desktop modules
// and the standard library, regardless of the removed backend interpreter.
test("desktop recovery helpers work without the Python backend", () => {
  execFileSync(
    "python3",
    ["-B", path.join(import.meta.dirname, "recovery-test.py")],
    {
      env: { ...process.env, CODEX_AGENTS_PYTHON: "/nonexistent" },
      stdio: "pipe",
    },
  );
});
