// Manual process integration: just test-guest-runner-processes.
// The fast tier tests the executor contract and runs the actual guest suite.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { stopSuiteProcesses } from "./run-guest-suite.mjs";

test("parallel modules keep ordered output and aggregate failures and skips", () => {
  const folder = mkdtempSync(path.join(os.tmpdir(), "guest-modules-"));
  try {
    writeFileSync(
      path.join(folder, "test_a.py"),
      "import unittest\nclass A(unittest.TestCase):\n def test_fail(self): self.fail('module failure')\n",
    );
    writeFileSync(
      path.join(folder, "test_b.py"),
      "import os,sys,unittest\nclass B(unittest.TestCase):\n @unittest.skip('fixture')\n def test_skip(self): pass\n def test_pass(self):\n  self.assertNotIn('LAYR_E2E_ROOT',os.environ)\n  with self.assertRaisesRegex(RuntimeError,'privileged command forbidden'):\n   sys.audit('subprocess.Popen','/usr/sbin/groupadd',[],None,None)\n",
    );
    const run = spawnSync(
      process.env.STUDIO_GUEST_PYTHON || "python3",
      ["-B", fileURLToPath(new URL("guest_suite.py", import.meta.url)), folder],
      {
        encoding: "utf8",
        timeout: 10000,
        cwd: folder,
        env: {
          ...process.env,
          STUDIO_GUEST_WORKERS: "2",
          LAYR_E2E_ROOT: "/not-for-contracts",
          TMPDIR: folder,
        },
      },
    );
    assert.equal(run.status, 1, run.stderr);
    assert.match(run.stderr, /module failure/);
    assert.ok(run.stderr.indexOf("test_a:") < run.stderr.indexOf("test_b:"));
    assert.match(run.stderr, /Ran 3 tests in /);
    assert.match(run.stderr, /FAILED \(failures=1, errors=0, skipped=1\)/);
  } finally {
    const leaked = stopSuiteProcesses(folder);
    rmSync(folder, { recursive: true, force: true });
    assert.deepEqual(leaked, [], "the worker scenario must stop its processes");
  }
});

test("a crashed module names unfinished modules and fails the run", () => {
  const folder = mkdtempSync(path.join(os.tmpdir(), "guest-crash-"));
  try {
    writeFileSync(
      path.join(folder, "test_crash.py"),
      "import os,unittest\nclass Crash(unittest.TestCase):\n def test_crash(self): os._exit(7)\n",
    );
    const run = spawnSync(
      process.env.STUDIO_GUEST_PYTHON || "python3",
      ["-B", fileURLToPath(new URL("guest_suite.py", import.meta.url)), folder],
      {
        encoding: "utf8",
        timeout: 10000,
        cwd: folder,
        env: { ...process.env, TMPDIR: folder },
      },
    );
    assert.equal(run.status, 1, run.stderr);
    assert.match(run.stderr, /module worker crashed; unfinished: test_crash/);
  } finally {
    const leaked = stopSuiteProcesses(folder);
    rmSync(folder, { recursive: true, force: true });
    assert.deepEqual(leaked, [], "the crash scenario must stop its processes");
  }
});
