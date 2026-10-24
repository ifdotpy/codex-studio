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
export async function startBridge(
  fakeSdk,
  prefix = "claude-bridge-",
  { env = {}, thread = {} } = {},
) {
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(os.tmpdir(), prefix)),
  );
  for (const name of await fs.readdir(BRIDGE)) {
    if (!name.endsWith(".mjs") || name.endsWith(".test.mjs")) continue;
    const source = await fs.readFile(path.join(BRIDGE, name), "utf8");
    await fs.writeFile(
      path.join(root, name),
      source.replaceAll("@anthropic-ai/claude-agent-sdk", "./fake.mjs"),
    );
  }
  await fs.writeFile(
    path.join(root, "fake.mjs"),
    fakeSdk.includes("export const resolveSettings") ||
      fakeSdk.includes("export function resolveSettings")
      ? fakeSdk
      : "export const resolveSettings=async()=>({effective:{}});\n" + fakeSdk,
  );
  await fs.symlink(
    path.join(BRIDGE, "node_modules"),
    path.join(root, "node_modules"),
  );
  const launch = () =>
    spawn(
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
          ...env,
        },
      },
    );
  let child;
  let stderr = "";
  const notes = [];
  const waiting = new Map();
  const notifications = new Set();
  const attach = () => {
    child = launch();
    const attached = child;
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.once("close", () => {
      if (child !== attached) return;
      for (const { reject } of waiting.values())
        reject(new Error("bridge closed before replying\n" + stderr));
    });
    createInterface({ input: child.stdout }).on("line", (line) => {
      const message = JSON.parse(line);
      if (message.method) notes.push(message);
      else waiting.get(message.id)?.resolve(message);
      for (const notify of notifications) notify();
    });
  };
  attach();
  let sequence = 0;
  const bridge = {
    root,
    notes,
    async call(method, params) {
      const id = ++sequence;
      let timer;
      try {
        const message = await new Promise((resolve, reject) => {
          waiting.set(id, { resolve, reject });
          timer = setTimeout(
            () => reject(new Error(method + " hung\n" + stderr)),
            HANG_GUARD_MS,
          );
          child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
        });
        if (message.error)
          throw Object.assign(new Error(message.error.message), {
            data: message.error.data,
          });
        return message.result;
      } finally {
        clearTimeout(timer);
        waiting.delete(id);
      }
    },
    answer(id, result) {
      child.stdin.write(JSON.stringify({ id, result }) + "\n");
    },
    async note(predicate) {
      let timer, notify;
      try {
        return await new Promise((resolve, reject) => {
          notify = () => {
            const found = notes.find(predicate);
            if (found) resolve(found);
          };
          notifications.add(notify);
          timer = setTimeout(
            () => reject(new Error("notification missing\n" + stderr)),
            HANG_GUARD_MS,
          );
          notify();
        });
      } finally {
        clearTimeout(timer);
        notifications.delete(notify);
      }
    },
    async restart() {
      child.stdin.end();
      if (child.exitCode === null)
        await new Promise((resolve) => child.once("exit", resolve));
      notes.length = 0;
      waiting.clear();
      stderr = "";
      attach();
      await bridge.call("thread/resume", { threadId: bridge.thread });
    },
    async close() {
      child.stdin.end();
      if (child.exitCode === null)
        await new Promise((resolve) => child.once("exit", resolve));
      await fs.rm(root, { recursive: true, force: true });
    },
  };
  bridge.thread = (
    await bridge.call("thread/start", {
      cwd: root,
      dynamicTools: [],
      ...thread,
    })
  ).thread.id;
  return bridge;
}
