const { test } = require("node:test");
const { execFileSync } = require("node:child_process");
const path = require("node:path");

const HELPER_TEST_TIMEOUT_MS = 30_000;
const root = path.resolve(__dirname, "../../../..");
for (const [name, args] of [
  [
    "desktop helpers import without backend modules",
    ["-B", path.join(__dirname, "retained-helpers-test.py")],
  ],
  [
    "guest uses the Rust supervisor without backend modules",
    [
      "-B",
      "-m",
      "unittest",
      "discover",
      "-s",
      path.join(root, "workspaces/runtime/apps/vm-guest"),
      "-p",
      "test_retained_boundary.py",
    ],
  ],
]) {
  test(name, () => {
    execFileSync("python3", args, {
      env: { ...process.env, CODEX_AGENTS_PYTHON: "/nonexistent" },
      stdio: "pipe",
      timeout: HELPER_TEST_TIMEOUT_MS,
    });
  });
}
