// Start the real bridge with a scripted Agent SDK. No model calls are made.
import { once } from "node:events";
import { spawn } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

// Maximum simulated wait and per-step advancement, both in milliseconds.
const VIRTUAL_NOTIFICATION_BUDGET_MS = 10_000;
const VIRTUAL_STEP_MS = 10;
const BRIDGE = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
import { HANG_GUARD_MS } from "./file-signals.mjs";
export { HANG_GUARD_MS };

// Copies the bridge sources to a temporary folder, replaces the SDK import with
// `fakeSdk` (module source text) and starts the bridge with one new thread.
export async function startBridge(
  fakeSdk,
  prefix = "claude-bridge-",
  { env = {}, thread = {}, clock = false, temporaryBase = os.tmpdir() } = {},
) {
  // SDK init always carries a native session id. Keep older scripted fixtures
  // faithful to that declaration; an explicit session_id still takes precedence.
  fakeSdk = fakeSdk.replace(
    /subtype:\s*(["'])init\1/g,
    "subtype:'init',session_id:options.resume||options.sessionId",
  );
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(temporaryBase, prefix)),
  );
  for (const name of await fs.readdir(BRIDGE)) {
    if (!name.endsWith(".mjs") || name.endsWith(".test.mjs")) continue;
    const source = await fs.readFile(path.join(BRIDGE, name), "utf8");
    await fs.writeFile(
      path.join(root, name),
      source.replaceAll(
        "@anthropic-ai/claude-agent-sdk",
        name === "history-index.ts"
          ? path.join(
              BRIDGE,
              "node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs",
            )
          : "./fake.mjs",
      ),
    );
  }
  await fs.writeFile(
    path.join(root, "fake.mjs"),
    `import { waitForFile as waitForFileSignal } from ${JSON.stringify(path.join(BRIDGE, "testing/file-signals.mjs"))};\n` +
      `const waitForFile=(file,options)=>waitForFileSignal(file,{...options,childIpc:true});\n` +
      (fakeSdk.includes("export const resolveSettings") ||
      fakeSdk.includes("export function resolveSettings")
        ? fakeSdk
        : "export const resolveSettings=async()=>({effective:{}});\n" +
          fakeSdk),
  );
  await fs.symlink(
    path.join(BRIDGE, "node_modules"),
    path.join(root, "node_modules"),
  );
  const launch = () =>
    spawn(
      process.execPath,
      [
        ...(clock
          ? ["--import", path.join(BRIDGE, "testing/virtual-time.mjs")]
          : []),
        path.join(root, "bridge.mjs"),
        path.join(root, "state"),
      ],
      {
        cwd: root,
        stdio: ["pipe", "pipe", "pipe", "ipc"],
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
  // One exit observation per launched child, registered at spawn so cleanup can
  // never miss an exit that already happened (including a signal exit).
  let exited;
  let stderr = "";
  const notes = [];
  const waiting = new Map();
  const notifications = new Set();
  const attach = () => {
    child = launch();
    const attached = child;
    exited = new Promise((resolve) => attached.once("exit", resolve));
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
  let signalSequence = 0;
  const bridge = {
    root,
    notes,
    get process() {
      return child;
    },
    async writeSignal(name, content = "") {
      const file = path.join(root, name);
      const temporary = path.join(root, `.signal-${++signalSequence}.tmp`);
      await fs.writeFile(temporary, content);
      await fs.rename(temporary, file);
      await new Promise((resolve, reject) => {
        child.send({ fixtureFile: file }, (error) =>
          error ? reject(error) : resolve(),
        );
      });
    },
    async advanceTime(ms) {
      if (child.exitCode !== null) return;
      const controller = new AbortController();
      const timer = setTimeout(
        () => controller.abort(new Error("virtual clock hung")),
        HANG_GUARD_MS,
      );
      try {
        const advanced = Promise.race([
          once(child, "message", { signal: controller.signal }),
          once(child, "exit", { signal: controller.signal }),
        ]);
        child.send({ advance: ms });
        await advanced;
      } finally {
        clearTimeout(timer);
        controller.abort();
      }
    },
    async advanceUntil(predicate) {
      // Drive virtual time while disk and RPC work completes; no wall-clock polls.
      for (
        let elapsed = 0;
        elapsed < VIRTUAL_NOTIFICATION_BUDGET_MS;
        elapsed += VIRTUAL_STEP_MS
      ) {
        const found = notes.find(predicate);
        if (found) return found;
        await bridge.advanceTime(VIRTUAL_STEP_MS);
      }
      throw new Error("notification missing after virtual deadline\n" + stderr);
    },
    async call(method, params) {
      const id = ++sequence;
      let timer;
      try {
        const message = await new Promise((resolve, reject) => {
          waiting.set(id, { resolve, reject });
          timer = setTimeout(
            () =>
              reject(
                new Error(
                  method +
                    " hung\n" +
                    stderr +
                    "\nRecent notifications: " +
                    JSON.stringify(notes.slice(-10)),
                ),
              ),
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
            () =>
              reject(
                new Error(
                  "notification missing\n" +
                    stderr +
                    "\nRecent notifications: " +
                    JSON.stringify(notes.slice(-10)),
                ),
              ),
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
      await stop();
      notes.length = 0;
      waiting.clear();
      stderr = "";
      attach();
      await bridge.call("thread/resume", { threadId: bridge.thread });
    },
    async close() {
      try {
        await stop();
      } finally {
        await fs.rm(root, { recursive: true, force: true });
      }
    },
  };
  // Ends the current child for close() and restart(): drives a virtual clock
  // until exit, reaps a child whose clock can no longer be driven, and always
  // observes the exit registered at spawn.
  async function stop() {
    child.stdin.end();
    // A failed virtual-clock scenario can leave teardown waiting on a frozen timer.
    // Always reap this fixture's child, including when the test itself timed out.
    const guard = setTimeout(() => child.kill("SIGKILL"), HANG_GUARD_MS);
    try {
      // Bounded shutdown timers run on the virtual clock; advance it until exit.
      // A clock that can no longer be driven cannot finish the child: reap it.
      if (clock)
        while (child.exitCode === null && child.signalCode === null)
          try {
            await bridge.advanceTime(VIRTUAL_STEP_MS);
          } catch {
            child.kill("SIGKILL");
            break;
          }
      await exited;
    } finally {
      clearTimeout(guard);
    }
  }
  bridge.thread = (
    await bridge.call("thread/start", {
      cwd: root,
      dynamicTools: [],
      ...thread,
    })
  ).thread.id;
  return bridge;
}
