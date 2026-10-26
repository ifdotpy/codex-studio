// The leak check of the guest suite runner (run-guest-suite.mts): it finds the processes
// that name the run's folder or work in it, stops them, and leaves other processes alone.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import type { Socket } from "node:net";
import { createServer } from "node:net";
import { stopSuiteProcesses, suiteProcesses } from "./run-guest-suite.mts";

const WAIT = "process.stdin.resume(); process.send('ready')";

async function start(args: string[], cwd: string) {
  const child = spawn(process.execPath, args, {
    cwd,
    detached: true,
    stdio: ["pipe", "ignore", "ignore", "ipc"],
  });
  const exited = once(child, "exit");
  assert.ok(child.pid);
  await once(child, "message");
  child.disconnect();
  return Object.assign(child, { exited, pid: child.pid });
}

function alive(pid: number) {
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
    assert.ok(named && working && stubborn && other);
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

test("cases within one module overlap and keep ordered output, failures and skips", async (t) => {
  const folder = mkdtempSync(path.join(os.tmpdir(), "guest-modules-"));
  const peers: Socket[] = [];
  const barrier = createServer((peer) => {
    peers.push(peer);
    if (peers.length === 2) peers.forEach((socket) => socket.end("ready"));
  });
  barrier.listen(0, "127.0.0.1");
  await once(barrier, "listening");
  t.after(() => {
    peers.forEach((peer) => peer.destroy());
    barrier.close();
    rmSync(folder, { recursive: true, force: true });
  });
  const address = barrier.address();
  assert.ok(address && typeof address !== "string");
  writeFileSync(
    path.join(folder, "test_a.py"),
    `import os,socket,sys,unittest
class A(unittest.TestCase):
 def setUp(self):
  with socket.create_connection(('127.0.0.1',${address.port}),timeout=5) as peer:
   self.assertEqual(peer.recv(5),b'ready')
 def test_fail(self): self.fail('case failure')
 @unittest.skip('fixture')
 def test_skip(self): pass
 def test_pass(self):
  self.assertNotIn('LAYR_E2E_ROOT',os.environ)
  with self.assertRaisesRegex(RuntimeError,'privileged command forbidden'):
   sys.audit('subprocess.Popen','/usr/sbin/groupadd',[],None,None)
`,
  );
  const run = spawn(
    process.env.STUDIO_GUEST_PYTHON || "python3",
    ["-B", fileURLToPath(new URL("guest_suite.py", import.meta.url)), folder],
    {
      timeout: 10000,
      env: {
        ...process.env,
        STUDIO_GUEST_WORKERS: "2",
        LAYR_E2E_ROOT: "/not-for-contracts",
      },
    },
  );
  let stderr = "";
  run.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  const [code] = await once(run, "close");
  assert.equal(code, 1, stderr);
  assert.match(stderr, /case failure/);
  assert.ok(
    stderr.indexOf("test_a.A.test_fail:") <
      stderr.indexOf("test_a.A.test_pass:"),
  );
  assert.match(stderr, /Ran 3 tests in /);
  assert.match(stderr, /FAILED \(failures=1, errors=0, skipped=1\)/);
});

test("a crashed case names unfinished cases and fails the run", () => {
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
    assert.match(
      run.stderr,
      /test worker crashed; unfinished: test_crash.Crash.test_crash/,
    );
  } finally {
    rmSync(folder, { recursive: true, force: true });
  }
});
