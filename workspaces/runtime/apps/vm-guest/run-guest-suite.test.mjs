// The leak check of the guest suite runner (run-guest-suite.mjs): it finds the processes
// that name the run's folder or work in it, stops them, and leaves other processes alone.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
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

// Refuse a global probe before it can inspect processes outside this fixture.
// Valid scoped probes still execute the actual operating-system commands.
function scopedProbeGuard(folder, pids) {
  const bin = path.join(folder, "probe-bin");
  const log = path.join(folder, "probes.jsonl");
  mkdirSync(bin);
  for (const [name, executable] of [
    ["ps", "/bin/ps"],
    ["lsof", "/usr/sbin/lsof"],
  ]) {
    writeFileSync(
      path.join(bin, name),
      `#!${process.execPath}
import { appendFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
const args=process.argv.slice(2);
appendFileSync(${JSON.stringify(log)},JSON.stringify({name:${JSON.stringify(name)},args})+'\\n');
const index=args.indexOf('-p');
const selected=index<0?[]:args[index+1].split(',').map(Number);
if(!selected.length || args.includes('-A') || selected.some(pid=>!${JSON.stringify(pids)}.includes(pid))) process.exit(91);
const result=spawnSync(${JSON.stringify(executable)},args,{stdio:'inherit'});
process.exit(result.status??92);
`,
      { mode: 0o700 },
    );
  }
  const previous = process.env.PATH;
  process.env.PATH = bin + path.delimiter + (previous ?? "");
  return {
    probes: () =>
      existsSync(log)
        ? readFileSync(log, "utf8").trim().split("\n").map(JSON.parse)
        : [],
    restore: () => {
      if (previous === undefined) delete process.env.PATH;
      else process.env.PATH = previous;
    },
  };
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
    const pids = [named, working, stubborn, other];
    const probes = scopedProbeGuard(folder, pids);
    try {
      const found = suiteProcesses(folder, { pids }).map(({ pid }) => pid);
      for (const pid of [named, working, stubborn])
        assert.ok(found.includes(pid), `${pid}`);
      assert.ok(!found.includes(other));
      const stopped = stopSuiteProcesses(folder, {
        exitGrace: 0,
        stopGrace: 25,
        pids,
      })
        .map(({ pid }) => pid)
        .sort();
      assert.deepEqual(stopped, [named, working, stubborn].sort());
      await Promise.all(children.slice(0, 3).map((child) => child.exited));
      for (const pid of [named, working, stubborn])
        assert.ok(!alive(pid), `${pid}`);
      assert.deepEqual(
        suiteProcesses(folder, { pids: [named, working, stubborn] }),
        [],
      );
      assert.ok(alive(other), "a process of another folder keeps running");
      assert.deepEqual(stopSuiteProcesses(folder, { exitGrace: 0, pids }), []);
      assert(probes.probes().some(({ name }) => name === "ps"));
      if (process.platform === "darwin") {
        const last = probes
          .probes()
          .filter(({ name }) => name === "lsof")
          .at(-1);
        assert(last, "the working-folder owner uses the real scoped lsof");
        assert.equal(
          last.args[last.args.indexOf("-p") + 1],
          String(other),
          "known-dead PIDs do not enter the final cwd probe",
        );
      }
    } finally {
      probes.restore();
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

test("empty and invalid PID selections never fall back to global probes", () => {
  const folder = mkdtempSync(path.join(os.tmpdir(), "guest-empty-scope-"));
  const probes = scopedProbeGuard(folder, []);
  try {
    assert.deepEqual(suiteProcesses(folder, { pids: [] }), []);
    assert.deepEqual(
      stopSuiteProcesses(folder, { pids: [], exitGrace: 0 }),
      [],
    );
    for (const pids of [
      null,
      "1",
      [0],
      [-1],
      [1.5],
      [NaN],
      [Infinity],
      ["1"],
      Array(1),
    ]) {
      assert.throws(() => suiteProcesses(folder, { pids }), /PID/);
      assert.throws(
        () => stopSuiteProcesses(folder, { pids, exitGrace: 0 }),
        /PID/,
      );
    }
    assert.deepEqual(
      probes.probes(),
      [],
      "no command runs for empty or invalid scope",
    );
  } finally {
    probes.restore();
    rmSync(folder, { recursive: true, force: true });
  }
});

test("a failed or interrupted scoped ps probe is not an empty selection", () => {
  const folder = mkdtempSync(path.join(os.tmpdir(), "guest-failed-probe-"));
  const probes = scopedProbeGuard(folder, [process.pid]);
  try {
    for (const script of [
      "process.stderr.write('fixture probe refused'); process.exit(1)",
      "process.exit(2)",
      "process.kill(process.pid, 'SIGTERM')",
    ]) {
      writeFileSync(
        path.join(folder, "probe-bin", "ps"),
        `#!${process.execPath}\n${script}\n`,
        { mode: 0o700 },
      );
      assert.throws(
        () => suiteProcesses(folder, { pids: [process.pid] }),
        /ps did not list/,
      );
    }
    writeFileSync(
      path.join(folder, "probe-bin", "ps"),
      `#!${process.execPath}\nprocess.stdout.write('999999999 fixture-process\\n')\n`,
      { mode: 0o700 },
    );
    assert.throws(
      () => stopSuiteProcesses(folder, { pids: [process.pid], exitGrace: 0 }),
      /ps did not list/,
    );
    assert.deepEqual(
      probes.probes(),
      [],
      "no cwd probe follows an unconfirmed ps result",
    );
  } finally {
    probes.restore();
    rmSync(folder, { recursive: true, force: true });
  }
});

// The Linux owner reads /proc directly and does not invoke lsof.
if (!existsSync("/proc/self/cwd"))
  test("a failed scoped cwd probe refuses cleanup without signals", async () => {
    const folder = mkdtempSync(path.join(os.tmpdir(), "guest-failed-cwd-"));
    const child = await start(["-e", WAIT], folder);
    const probes = scopedProbeGuard(folder, [child.pid]);
    const originalKill = process.kill;
    const signals = [];
    process.kill = (pid, signal) => {
      if (signal !== 0) signals.push({ pid, signal });
      return originalKill.call(process, pid, signal);
    };
    try {
      // Restrict executable lookup too, so a missing lsof cannot use another copy.
      process.env.PATH = path.join(folder, "probe-bin");
      for (const script of [
        `#!${process.execPath}\nprocess.stderr.write('fixture cwd probe refused'); process.exit(1)\n`,
        `#!${process.execPath}\nprocess.exit(2)\n`,
        `#!${process.execPath}\nprocess.kill(process.pid, 'SIGTERM')\n`,
        "#!/missing-fixture-interpreter\n",
      ]) {
        writeFileSync(path.join(folder, "probe-bin", "lsof"), script, {
          mode: 0o700,
        });
        assert.throws(
          () =>
            stopSuiteProcesses(folder, {
              pids: [child.pid],
              exitGrace: 0,
              stopGrace: 0,
            }),
          /lsof did not list/,
        );
        assert(
          alive(child.pid),
          "a failed cwd proof must not signal the owned child",
        );
        assert.deepEqual(
          signals,
          [],
          "cleanup sends no signals without the cwd proof",
        );
      }
    } finally {
      process.kill = originalKill;
      probes.restore();
      child.kill("SIGKILL");
      await child.exited;
      rmSync(folder, { recursive: true, force: true });
    }
  });

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
