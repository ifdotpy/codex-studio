// Manual process integration: just test-guest-runner-processes.
// The fast tier checks the executor contract and runs the actual guest suite.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { createServer } from "node:net";
import { stopSuiteProcesses } from "./run-guest-suite.mjs";

test("cases within one module overlap and keep ordered output, failures and skips", async (t) => {
  const folder = mkdtempSync(path.join(os.tmpdir(), "guest-modules-"));
  const peers = [];
  const barrier = createServer((peer) => {
    peers.push(peer);
    if (peers.length === 2) peers.forEach((socket) => socket.end("ready"));
  });
  barrier.listen(0, "127.0.0.1");
  await once(barrier, "listening");
  t.after(() => {
    peers.forEach((peer) => peer.destroy());
    barrier.close();
    const leaked = stopSuiteProcesses(folder);
    rmSync(folder, { recursive: true, force: true });
    assert.deepEqual(leaked, [], "the worker scenario must stop its processes");
  });
  writeFileSync(
    path.join(folder, "test_a.py"),
    `import os,socket,sys,unittest
class A(unittest.TestCase):
 def setUp(self):
  with socket.create_connection(('127.0.0.1',${barrier.address().port}),timeout=5) as peer:
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
      cwd: folder,
      env: {
        ...process.env,
        TMPDIR: folder,
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
      {
        encoding: "utf8",
        timeout: 10000,
        cwd: folder,
        env: { ...process.env, TMPDIR: folder },
      },
    );
    assert.equal(run.status, 1, run.stderr);
    assert.match(
      run.stderr,
      /test worker crashed; unfinished: test_crash.Crash.test_crash/,
    );
  } finally {
    const leaked = stopSuiteProcesses(folder);
    rmSync(folder, { recursive: true, force: true });
    assert.deepEqual(leaked, [], "the crash scenario must stop its processes");
  }
});
