// The leak check of the guest suite runner (run-guest-suite.mjs): it finds the processes
// that name the run's folder or work in it, stops them, and leaves other processes alone.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { stopSuiteProcesses, suiteProcesses } from "./run-guest-suite.mjs";

const WAIT = "process.send('ready'); setTimeout(() => {}, 60000)";

async function start(args, cwd) {
  const child = spawn(process.execPath, args, {
    cwd,
    detached: true,
    stdio: ["ignore", "ignore", "ignore", "ipc"],
  });
  child.exited = once(child, "exit");
  await once(child, "message");
  child.disconnect();
  return child;
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

test(
  "processes that a suite run leaves are found and stopped, others stay",
  { timeout: 10000 },
  async () => {
    const folder = mkdtempSync(path.join(os.tmpdir(), "guest-suite-"));
    const work = path.join(folder, "work");
    mkdirSync(work);
    const outside = mkdtempSync(path.join(os.tmpdir(), "guest-other-"));
    const children = await Promise.all([
      start(["-e", WAIT, path.join(folder, "state")], outside),
      start(["-e", WAIT], work),
      start(
        ["-e", `process.on("SIGTERM", () => {}); ${WAIT}`, folder],
        outside,
      ),
      start(["-e", WAIT], outside),
    ]);
    const [named, working, stubborn, other] = children.map(({ pid }) => pid);
    try {
      const found = suiteProcesses(folder).map(({ pid }) => pid);
      for (const pid of [named, working, stubborn])
        assert.ok(found.includes(pid), `${pid}`);
      assert.ok(!found.includes(other));
      const stopped = stopSuiteProcesses(folder, {
        exitGrace: 0,
        stopGrace: 25,
      })
        .map(({ pid }) => pid)
        .sort();
      assert.deepEqual(stopped, [named, working, stubborn].sort());
      await Promise.all(children.slice(0, 3).map((child) => child.exited));
      for (const pid of [named, working, stubborn])
        assert.ok(!alive(pid), `${pid}`);
      assert.ok(alive(other), "a process of another folder keeps running");
      assert.deepEqual(stopSuiteProcesses(folder, { exitGrace: 0 }), []);
    } finally {
      for (const pid of [named, working, stubborn, other])
        try {
          process.kill(pid, "SIGKILL");
        } catch {
          // It already ended.
        }
      await Promise.all(children.map((child) => child.exited));
      rmSync(folder, { recursive: true, force: true });
      rmSync(outside, { recursive: true, force: true });
    }
  },
);

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
        env: {
          ...process.env,
          STUDIO_GUEST_WORKERS: "2",
          LAYR_E2E_ROOT: "/not-for-contracts",
        },
      },
    );
    assert.equal(run.status, 1, run.stderr);
    assert.match(run.stderr, /module failure/);
    assert.ok(run.stderr.indexOf("test_a:") < run.stderr.indexOf("test_b:"));
    assert.match(run.stderr, /Ran 3 tests in /);
    assert.match(run.stderr, /FAILED \(failures=1, errors=0, skipped=1\)/);
  } finally {
    rmSync(folder, { recursive: true, force: true });
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
      { encoding: "utf8", timeout: 10000 },
    );
    assert.equal(run.status, 1, run.stderr);
    assert.match(run.stderr, /module worker crashed; unfinished: test_crash/);
  } finally {
    rmSync(folder, { recursive: true, force: true });
  }
});
