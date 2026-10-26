import { test } from "vitest";
import assert from "node:assert/strict";
import { fork } from "node:child_process";
import { once } from "node:events";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { HANG_GUARD_MS } from "./testing/file-signals.ts";

test("test-side file waits preserve IPC while awaiting a delayed parent reply", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-file-signals-"));
  let child, replyTimer;
  try {
    const file = path.join(root, "ready");
    const script = path.join(root, "child.mjs");
    await fs.writeFile(file, "ready");
    await fs.writeFile(
      script,
      `import { once } from 'node:events';
import { waitForFile } from ${JSON.stringify(new URL("./testing/file-signals.ts", import.meta.url).href)};
async function run() {
const reply = once(process, 'message');
await waitForFile(${JSON.stringify(file)});
process.send('ready');
const [message] = await reply;
process.send(message);
process.disconnect();
}
run().catch(error => { console.error(error); process.exitCode = 1; });
`,
    );
    child = fork(script, [], {
      stdio: ["ignore", "ignore", "pipe", "ipc"],
      execArgv: [],
    });
    const messages = [];
    let stderr = "";
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.on("message", (message) => {
      messages.push(message);
      if (message === "ready") {
        // Leave the child waiting only on its IPC channel, as a Vitest worker
        // does while awaiting an RPC reply from the test runner.
        replyTimer = setTimeout(() => {
          if (child.connected) child.send("replied", () => {});
        }, 100);
      }
    });
    const [code, signal] = await once(child, "exit", {
      signal: AbortSignal.timeout(HANG_GUARD_MS),
    });
    assert.equal(signal, null, stderr);
    assert.equal(code, 0, stderr);
    assert.deepEqual(messages, ["ready", "replied"]);
  } finally {
    clearTimeout(replyTimer);
    if (child && child.exitCode === null && child.signalCode === null) {
      const exited = once(child, "exit");
      child.kill("SIGKILL");
      await exited;
    }
    await fs.rm(root, { recursive: true, force: true });
  }
});
