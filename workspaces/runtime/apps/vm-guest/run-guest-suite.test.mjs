// The leak check of the guest suite runner (run-guest-suite.mjs): it finds the processes
// that name the run's folder or work in it, stops them, and leaves other processes alone.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
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
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import {
  stopSuiteProcesses,
  suiteProcesses,
  suiteTemporaryBase,
} from "./run-guest-suite.mjs";

function runnerFixture({ probeFails = false, suiteFails = false } = {}) {
  const folder = mkdtempSync(path.join(os.tmpdir(), "guest-runner-boundary-"));
  const log = path.join(folder, "calls.jsonl");
  const calls = () =>
    existsSync(log)
      ? readFileSync(log, "utf8").trim().split("\n").map(JSON.parse)
      : [];
  try {
    for (const name of ["python", "ps", "lsof"]) {
      const script =
        name === "python"
          ? `if(process.argv.includes('-c')) process.stdout.write('1\\n');
else { appendFileSync(${JSON.stringify(log)},JSON.stringify({name:'suite',temporary:process.env.TMPDIR})+'\\n'); process.stderr.write('fixture Python result\\nRan 1 tests in 0.001s\\n${suiteFails ? "FAILED (failures=1)" : "OK"}\\n'); process.exit(${suiteFails ? 1 : 0}); }`
          : `const args=process.argv.slice(2);
appendFileSync(${JSON.stringify(log)},JSON.stringify({name:${JSON.stringify(name)},args})+'\\n');
if (${probeFails === true || probeFails === name} || (${JSON.stringify(name)}==='ps' && (args.includes('-A') || args[args.indexOf('-u')+1]!==${JSON.stringify(String(os.userInfo().uid))}))) {process.stderr.write('fixture cleanup probe refused');process.exit(2);}`;
      writeFileSync(
        path.join(folder, name),
        `#!${process.execPath}\nimport {appendFileSync} from 'node:fs';\n${script}\n`,
        { mode: 0o700 },
      );
    }
    const run = spawnSync(
      process.execPath,
      [fileURLToPath(new URL("run-guest-suite.mjs", import.meta.url))],
      {
        encoding: "utf8",
        timeout: 10000,
        env: {
          ...process.env,
          PATH: folder + path.delimiter + (process.env.PATH ?? ""),
          STUDIO_GUEST_PYTHON: path.join(folder, "python"),
          STUDIO_SUPERVISOR_BINARY: process.execPath,
        },
      },
    );
    return { run, calls: calls() };
  } finally {
    // A failed cleanup probe leaves its own temporary folder for inspection.
    // The fixture records that exact path before it can fail.
    for (const call of calls()) {
      if (call.name !== "suite") continue;
      assert.match(path.basename(call.temporary), /^g[^/]+$/);
      assert.equal(path.dirname(call.temporary), suiteTemporaryBase());
      rmSync(call.temporary, { recursive: true, force: true });
    }
    rmSync(folder, { recursive: true, force: true });
  }
}

test("the actual runner limits whole-suite cleanup probes to its operator UID", () => {
  const { run, calls } = runnerFixture();
  assert.equal(run.status, 0, run.stderr);
  const probe = calls.find(({ name }) => name === "ps");
  assert(probe, "normal completion still checks for leaked processes");
  assert(!probe.args.includes("-A"));
  assert.equal(
    probe.args[probe.args.indexOf("-u") + 1],
    String(os.userInfo().uid),
  );
  assert.match(run.stderr, /fixture Python result/);
});

test("the actual runner preserves the Python failure before a cleanup probe failure", () => {
  const { run } = runnerFixture({ probeFails: true, suiteFails: true });
  assert.notEqual(run.status, 0);
  assert.match(run.stderr, /fixture Python result/);
  assert.match(run.stderr, /FAILED \(failures=1\)/);
  assert.match(run.stderr, /ps did not list the processes/);
  assert(
    run.stderr.indexOf("fixture Python result") <
      run.stderr.indexOf("ps did not list the processes"),
  );
});

// Linux uses /proc instead of lsof for its working-folder proof.
if (!existsSync("/proc/self/cwd"))
  test("the actual runner refuses an unknown whole-suite working-folder probe", () => {
    const { run, calls } = runnerFixture({ probeFails: "lsof" });
    assert.notEqual(run.status, 0);
    assert(calls.some(({ name }) => name === "lsof"));
    assert.match(run.stderr, /fixture Python result/);
    assert.match(run.stderr, /lsof did not list/);
  });

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

function moduleExecutor(folder, broken = false) {
  const script = `
import importlib.util, multiprocessing, sys
from pathlib import Path
source, directory = sys.argv[1:]
spec = importlib.util.spec_from_file_location("guest_suite_fixture", source)
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
class Executor:
 def __init__(self, *, max_workers, mp_context, initializer, initargs):
  assert max_workers == 2
  assert mp_context.get_start_method() == "spawn"
  assert initializer is runner.prepare
  assert initargs == (str(Path(directory).resolve()),)
  assert not multiprocessing.active_children()
 def __enter__(self): return self
 def __exit__(self, *error):
  assert not multiprocessing.active_children()
  print("fixture executor closed", file=sys.stderr)
 def map(self, function, names):
  assert function is runner.run_module
  assert names == ${broken ? '["test_a", "test_crash", "test_z"]' : '["test_a", "test_b"]'}
  ${broken ? 'def crashed():\n   yield function("test_a")\n   raise runner.BrokenProcessPool("fixture worker exit")\n  return crashed()' : "return map(function, names)"}
sys.argv = [source, directory]
sys.exit(runner.main(executor_factory=Executor))
`;
  return spawnSync(
    process.env.STUDIO_GUEST_PYTHON || "python3",
    [
      "-B",
      "-c",
      script,
      fileURLToPath(new URL("guest_suite.py", import.meta.url)),
      folder,
    ],
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
}

test("the module executor keeps ordered output and aggregates failures and skips", () => {
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
    const run = moduleExecutor(folder);
    assert.equal(
      run.status,
      1,
      `${run.error?.stack ?? ""}\n${run.signal ?? ""}\n${run.stderr}`,
    );
    assert.match(run.stderr, /module failure/);
    assert.ok(run.stderr.indexOf("test_a:") < run.stderr.indexOf("test_b:"));
    assert.match(run.stderr, /Ran 3 tests in /);
    assert.match(run.stderr, /FAILED \(failures=1, errors=0, skipped=1\)/);
    assert.equal(run.stderr.match(/fixture executor closed/g)?.length, 1);
  } finally {
    rmSync(folder, { recursive: true, force: true });
  }
});

test("a broken executor closes its pool and names unfinished modules", () => {
  const folder = mkdtempSync(path.join(os.tmpdir(), "guest-crash-"));
  try {
    for (const name of ["test_a", "test_z"])
      writeFileSync(
        path.join(folder, `${name}.py`),
        "import unittest\nclass Complete(unittest.TestCase):\n def test_pass(self): pass\n",
      );
    writeFileSync(
      path.join(folder, "test_crash.py"),
      "import unittest\nclass Crash(unittest.TestCase):\n def test_crash(self): pass\n",
    );
    const run = moduleExecutor(folder, true);
    assert.equal(
      run.status,
      1,
      `${run.error?.stack ?? ""}\n${run.signal ?? ""}\n${run.stderr}`,
    );
    assert.match(run.stderr, /test_a: 1 tests in /);
    assert.match(
      run.stderr,
      /module worker crashed; unfinished: test_crash, test_z/,
    );
    assert.equal(run.stderr.match(/fixture executor closed/g)?.length, 1);
  } finally {
    rmSync(folder, { recursive: true, force: true });
  }
});
