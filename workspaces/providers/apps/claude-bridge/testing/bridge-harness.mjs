// Start the real bridge with a scripted Agent SDK. No model calls are made.
import { spawn } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const BRIDGE = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
export const HANG_GUARD_MS = 10_000;

// Copies the bridge sources to a temporary folder, replaces the SDK import with
// `fakeSdk` (module source text) and starts the bridge with one new thread.
export async function startBridge(fakeSdk, prefix = "claude-bridge-") {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), prefix));
  for (const name of await fs.readdir(BRIDGE)) {
    if (!name.endsWith(".mjs") || name.endsWith(".test.mjs")) continue;
    const source = await fs.readFile(path.join(BRIDGE, name), "utf8");
    await fs.writeFile(
      path.join(root, name),
      source.replaceAll("@anthropic-ai/claude-agent-sdk", "./fake.mjs"),
    );
  }
  await fs.writeFile(path.join(root, "fake.mjs"), fakeSdk);
  await fs.symlink(
    path.join(BRIDGE, "node_modules"),
    path.join(root, "node_modules"),
  );
  const child = spawn(
    process.execPath,
    [path.join(root, "bridge.mjs"), path.join(root, "state")],
    {
      cwd: root,
      stdio: ["pipe", "pipe", "pipe"],
      env: {
        ...process.env,
        STUDIO_CLAUDE_ACCOUNT: "test@example.test",
        STUDIO_CLAUDE_IDLE_SECONDS: "60",
        CLAUDE_CONFIG_DIR: path.join(root, "native-config"),
      },
    },
  );
  let stderr = "";
  child.stderr.on("data", (chunk) => (stderr += chunk));
  const notes = [];
  const waiting = new Map();
  let wake = () => {};
  createInterface({ input: child.stdout }).on("line", (line) => {
    const message = JSON.parse(line);
    if (message.method) notes.push(message);
    else waiting.get(message.id)?.(message);
    wake();
  });
  let sequence = 0;
  const bridge = {
    root,
    notes,
    async call(method, params) {
      const id = ++sequence;
      const reply = new Promise((resolve) => waiting.set(id, resolve));
      child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
      const message = await Promise.race([
        reply,
        new Promise((_, reject) =>
          setTimeout(
            () => reject(new Error(method + " hung\n" + stderr)),
            HANG_GUARD_MS,
          ),
        ),
      ]);
      if (message.error) throw new Error(message.error.message);
      return message.result;
    },
    async note(predicate) {
      const deadline = Date.now() + HANG_GUARD_MS;
      for (;;) {
        const found = notes.find(predicate);
        if (found) return found;
        if (Date.now() > deadline)
          throw new Error("notification missing\n" + stderr);
        await new Promise((resolve) => {
          wake = resolve;
          setTimeout(resolve, 50);
        });
      }
    },
    async close() {
      child.stdin.end();
      if (child.exitCode === null)
        await new Promise((resolve) => child.once("exit", resolve));
      await fs.rm(root, { recursive: true, force: true });
    },
  };
  bridge.thread = (
    await bridge.call("thread/start", { cwd: root, dynamicTools: [] })
  ).thread.id;
  return bridge;
}
