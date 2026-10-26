// Runs the guest Mac sync unittest suite under node:test so the fast tier
// counts its result.
import { execFileSync } from "node:child_process";
import { dirname } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

test("guest Mac sync reads at most one chunk per request", () => {
  execFileSync(
    "python3",
    [
      "-B",
      "-m",
      "unittest",
      "discover",
      "-s",
      dirname(fileURLToPath(import.meta.url)),
      "-p",
      "test_mac_sync.py",
    ],
    { stdio: "inherit" },
  );
});
