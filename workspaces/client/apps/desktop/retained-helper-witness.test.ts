import { test } from "node:test";
import { execFileSync } from "node:child_process";
import path from "node:path";

const HELPER_TEST_TIMEOUT_MS = 30_000;
const root = path.resolve(import.meta.dirname, "../../../..");
for (const [name, args] of [
  [
    "desktop helpers import without backend modules",
    ["-B", path.join(import.meta.dirname, "retained-helpers-test.py")],
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
] satisfies [string, string[]][]) {
  test(name, () => {
    execFileSync("python3", args, {
      env: { ...process.env, CODEX_AGENTS_PYTHON: "/nonexistent" },
      stdio: "pipe",
      timeout: HELPER_TEST_TIMEOUT_MS,
    });
  });
}
