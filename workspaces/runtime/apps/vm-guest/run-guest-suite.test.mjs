// The leak check of the guest suite runner (run-guest-suite.mjs): it finds the processes
// that name the run's folder or work in it, stops them, and leaves other processes alone.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import { stopSuiteProcesses, suiteProcesses } from "./run-guest-suite.mjs";

const WAIT = "setTimeout(() => {}, 60000)";

function start(args, cwd) {
  const child = spawn(process.execPath, args, {
    cwd,
    detached: true,
    stdio: "ignore",
  });
  child.unref();
  return child.pid;
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

test("processes that a suite run leaves are found and stopped, others stay", async () => {
  const folder = mkdtempSync(path.join(os.tmpdir(), "guest-suite-"));
  const work = path.join(folder, "work");
  mkdirSync(work);
  const outside = mkdtempSync(path.join(os.tmpdir(), "guest-other-"));
  const named = start(["-e", WAIT, path.join(folder, "state")], outside);
  const working = start(["-e", WAIT], work);
  const stubborn = start(
    ["-e", `process.on("SIGTERM", () => {}); ${WAIT}`, folder],
    outside,
  );
  const other = start(["-e", WAIT], outside);
  try {
    // Give the children time to start and change into their folders.
    await new Promise((resolve) => setTimeout(resolve, 500));
    const found = suiteProcesses(folder).map(({ pid }) => pid);
    for (const pid of [named, working, stubborn])
      assert.ok(found.includes(pid), `${pid}`);
    assert.ok(!found.includes(other));
    const stopped = stopSuiteProcesses(folder, { exitGrace: 0, stopGrace: 500 })
      .map(({ pid }) => pid)
      .sort();
    assert.deepEqual(stopped, [named, working, stubborn].sort());
    await new Promise((resolve) => setTimeout(resolve, 500));
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
    rmSync(folder, { recursive: true, force: true });
    rmSync(outside, { recursive: true, force: true });
  }
});
