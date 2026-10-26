const { test } = require("node:test");
const { execFileSync } = require("node:child_process");
const path = require("node:path");

// Node owns the fast-tier summary; the helper suite uses only desktop modules
// and the standard library, regardless of the removed backend interpreter.
test("desktop recovery helpers work without the Python backend", () => {
  execFileSync("python3", ["-B", path.join(__dirname, "recovery-test.py")], {
    env: { ...process.env, CODEX_AGENTS_PYTHON: "/nonexistent" },
    stdio: "pipe",
  });
});
