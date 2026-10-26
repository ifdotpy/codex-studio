import type {
  CallToolResult,
  TextContent,
} from "@modelcontextprotocol/sdk/types.js";
export type FixtureToolResult = CallToolResult & { content: TextContent[] };
// Start the real bridge with a scripted Agent SDK. No model calls are made.
import { once } from "node:events";
import { spawn } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

import type { SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import type {
  BridgeEmpty,
  BridgeMethod,
  BridgeNotification,
  BridgeParams,
  BridgeResponse,
  BridgeResult,
  BridgeRpcId,
  BridgeServerRequest,
  BridgeServerMethod,
  BridgeServerResult,
} from "../bridge-protocol.ts";

// Fixture events share the transport but are not part of the production protocol.
export interface BridgeFixtureNotifications {
  "fixture/callback": { value?: unknown; error?: string; decodes: number };
  "fixture/clockVisits": { phase: string; visits: number; at: number };
  "fixture/mode": {
    mode: import("@anthropic-ai/claude-agent-sdk").PermissionMode;
  };
  "fixture/options": {
    allowed: boolean;
    mode: import("@anthropic-ai/claude-agent-sdk").PermissionMode;
  };
  "fixture/permission": import("@anthropic-ai/claude-agent-sdk").PermissionResult;
  "fixture/second-permission": import("@anthropic-ai/claude-agent-sdk").PermissionResult;
  "fixture/input": { text: string } | SDKUserMessage["message"];
  "fixture/signal": { name: string };
  "fixture/result-ready": { token: string };
  "fixture/query": {
    resume?: string | undefined;
    sessionId?: string | undefined;
  };
  "fixture/shutdownTiming": { durationMs: number };
  "fixture/persistFailureSettled": BridgeEmpty;
  "fixture/stallSaveBlocked": BridgeEmpty;
  "fixture/interrupt": BridgeEmpty;
  "fixture/consumed-clear": BridgeEmpty;
  "fixture/anchor": BridgeEmpty;
  "fixture/crash": BridgeEmpty;
}
export type BridgeFixtureNotification = {
  [M in keyof BridgeFixtureNotifications]: {
    method: M;
    params: BridgeFixtureNotifications[M];
  };
}[keyof BridgeFixtureNotifications];
export type BridgeNote =
  | BridgeNotification
  | BridgeServerRequest
  | BridgeFixtureNotification;
export type BridgeNoteFor<M extends BridgeNote["method"]> = Extract<
  BridgeNote,
  { method: M }
>;

function hasMethod<M extends BridgeNote["method"]>(
  entry: BridgeNote,
  method: M,
): entry is BridgeNoteFor<M> {
  return entry.method === method;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function parseBridgeMessage(value: unknown): BridgeNote | BridgeResponse {
  if (!isRecord(value)) throw new Error("Invalid bridge message envelope");
  const validId = typeof value.id === "string" || typeof value.id === "number";
  if (typeof value.method === "string") {
    if (!("params" in value) || (value.id !== undefined && !validId))
      throw new Error("Invalid bridge notification envelope");
  } else if (
    !validId ||
    "result" in value === "error" in value ||
    ("error" in value &&
      (!isRecord(value.error) ||
        typeof value.error.message !== "string" ||
        typeof value.error.code !== "number"))
  ) {
    throw new Error("Invalid bridge response envelope");
  }
  // The child is the copied program under test and its scripted SDK fixture.
  // Their payloads use the TypeScript protocol owner; this boundary checks the
  // envelope and trusts those controlled producers for the payload fields.
  return value as BridgeNote | BridgeResponse;
}

// Virtual milliseconds per clock step.
const VIRTUAL_STEP_MS = 10;
// Longest real wait for a new bridge line between two clock steps.
const REAL_STEP_WAIT_MS = 10;
const BRIDGE = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
import { HANG_GUARD_MS } from "./file-signals.ts";
export { HANG_GUARD_MS };

// Copies the bridge sources to a temporary folder, replaces the SDK import with
// the fixture module and starts the bridge with one new thread.
export async function startBridge(
  fakeSdk: URL,
  prefix = "claude-bridge-",
  {
    env = {},
    thread = {},
    clock = false,
    temporaryBase = os.tmpdir(),
  }: {
    env?: NodeJS.ProcessEnv;
    thread?: Partial<BridgeParams<"thread/start">>;
    clock?: boolean;
    temporaryBase?: string;
  } = {},
) {
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(temporaryBase, prefix)),
  );
  for (const name of await fs.readdir(BRIDGE)) {
    if (!/\.(mjs|mts|ts)$/.test(name) || /\.test\.(mjs|mts|ts)$/.test(name))
      continue;
    const destination = path.join(root, name);
    if (name === "bridge.ts" || name === "refresh-auth.ts") {
      const source = await fs.readFile(path.join(BRIDGE, name), "utf8");
      await fs.writeFile(
        destination,
        source.replaceAll("@anthropic-ai/claude-agent-sdk", "./fake.mts"),
      );
    } else await fs.copyFile(path.join(BRIDGE, name), destination);
  }
  await fs.cp(path.join(BRIDGE, "fixtures"), path.join(root, "fixtures"), {
    recursive: true,
  });
  await fs.mkdir(path.join(root, "testing"));
  await fs.copyFile(
    path.join(BRIDGE, "testing/file-signals.ts"),
    path.join(root, "testing/file-signals.ts"),
  );
  const sourceFile = fileURLToPath(fakeSdk);
  const relativeFile = path.relative(BRIDGE, sourceFile);
  const fixtureFile =
    relativeFile.startsWith("..") || path.isAbsolute(relativeFile)
      ? path.basename(sourceFile)
      : relativeFile;
  await fs.copyFile(sourceFile, path.join(root, fixtureFile));
  await fs.writeFile(
    path.join(root, "fake.mts"),
    `export * from ${JSON.stringify("./" + fixtureFile.split(path.sep).join("/"))};\n`,
  );
  await fs.symlink(
    path.join(BRIDGE, "node_modules"),
    path.join(root, "node_modules"),
  );
  await fs.writeFile(
    path.join(root, "package.json"),
    JSON.stringify({ type: "module" }),
  );
  const launch = () =>
    spawn(
      process.execPath,
      [
        ...(clock
          ? ["--import", path.join(BRIDGE, "testing/virtual-time.ts")]
          : []),
        path.join(root, "bridge.ts"),
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
  let child: ReturnType<typeof launch>;
  // One exit observation per launched child, registered at spawn so cleanup can
  // never miss an exit that already happened (including a signal exit).
  let exited: Promise<void>;
  // Closed stdio means every bridge line has been read.
  let closed: Promise<void>;
  let stderr = "";
  const notes: BridgeNote[] = [];
  const waiting = new Map<
    BridgeRpcId,
    { resolve: (value: BridgeResponse) => void; reject: (error: Error) => void }
  >();
  const notifications = new Set<() => void>();
  const attach = () => {
    child = launch();
    const attached = child;
    exited = new Promise<void>((resolve) =>
      attached.once("exit", () => resolve()),
    );
    closed = new Promise<void>((resolve) =>
      attached.once("close", () => resolve()),
    );
    child.stderr?.on("data", (chunk) => (stderr += chunk));
    child.once("close", () => {
      if (child !== attached) return;
      for (const { reject } of waiting.values())
        reject(new Error("bridge closed before replying\n" + stderr));
    });
    createInterface({ input: child.stdout ?? process.stdin }).on(
      "line",
      (line) => {
        const value: unknown = JSON.parse(line);
        const message = parseBridgeMessage(value);
        if ("method" in message) notes.push(message);
        else waiting.get(message.id)?.resolve(message);
        for (const notify of notifications) notify();
      },
    );
  };
  attach();
  let sequence = 0;
  let signalSequence = 0;
  function advanceUntil<N extends BridgeNote>(
    predicate: (note: BridgeNote) => note is N,
  ): Promise<N>;
  function advanceUntil(
    predicate: (note: BridgeNote) => unknown,
  ): Promise<BridgeNote>;
  function advanceUntil<M extends BridgeNote["method"]>(
    method: M,
    predicate?: (note: BridgeNoteFor<M>) => unknown,
  ): Promise<BridgeNoteFor<M>>;
  async function advanceUntil<M extends BridgeNote["method"]>(
    methodOrPredicate: M | ((note: BridgeNote) => unknown),
    filter?: (note: BridgeNoteFor<M>) => unknown,
  ): Promise<BridgeNote> {
    const predicate =
      typeof methodOrPredicate === "function"
        ? methodOrPredicate
        : (entry: BridgeNote) =>
            hasMethod(entry, methodOrPredicate) && (!filter || filter(entry));
    // Drive virtual time while real disk and RPC work completes. That work
    // takes real time, so the deadline is real time, not a step count, and
    // each step waits for a new bridge line or a short real interval.
    const deadline = performance.now() + HANG_GUARD_MS;
    for (;;) {
      const found = notes.find(predicate);
      if (found) return found;
      if (childExited()) {
        // An exited bridge cannot produce the notification. Read its last
        // lines, then fail at once instead of at the deadline.
        await waitReal(closed, deadline - performance.now());
        const last = notes.find(predicate);
        if (last) return last;
        throw new Error(
          `notification missing: bridge exited (code ${child.exitCode}, signal ${child.signalCode})\n` +
            stderr,
        );
      }
      if (performance.now() >= deadline)
        throw new Error("notification missing after deadline\n" + stderr);
      await bridge.advanceTime(VIRTUAL_STEP_MS);
      const line = new Promise<void>((resolve) => {
        const done = () => {
          notifications.delete(done);
          resolve();
        };
        notifications.add(done);
      });
      await waitReal(line, REAL_STEP_WAIT_MS);
    }
  }
  function childExited() {
    return child.exitCode !== null || child.signalCode !== null;
  }
  function note<N extends BridgeNote>(
    predicate: (note: BridgeNote) => note is N,
  ): Promise<N>;
  function note(predicate: (note: BridgeNote) => unknown): Promise<BridgeNote>;
  function note<M extends BridgeNote["method"]>(
    method: M,
    predicate?: (note: BridgeNoteFor<M>) => unknown,
  ): Promise<BridgeNoteFor<M>>;
  async function note<M extends BridgeNote["method"]>(
    methodOrPredicate: M | ((note: BridgeNote) => unknown),
    filter?: (note: BridgeNoteFor<M>) => unknown,
  ): Promise<BridgeNote> {
    const predicate =
      typeof methodOrPredicate === "function"
        ? methodOrPredicate
        : (entry: BridgeNote) =>
            hasMethod(entry, methodOrPredicate) && (!filter || filter(entry));
    let timer: ReturnType<typeof setTimeout> | undefined;
    let notify = () => {};
    try {
      return await new Promise<BridgeNote>((resolve, reject) => {
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
  }
  const bridge = {
    root,
    notes,
    thread: "",
    get process() {
      return child;
    },
    async writeSignal(name: string, content = "") {
      const file = path.join(root, name);
      const temporary = path.join(root, `.signal-${++signalSequence}.tmp`);
      await fs.writeFile(temporary, content);
      await fs.rename(temporary, file);
      await new Promise<void>((resolve, reject) => {
        child.send({ fixtureFile: file }, (error) =>
          error ? reject(error) : resolve(),
        );
      });
    },
    async advanceTime(ms: number) {
      if (childExited()) return;
      const controller = new AbortController();
      const timer = setTimeout(
        () => controller.abort(new Error("virtual clock hung\n" + stderr)),
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
    advanceUntil,
    async call<M extends BridgeMethod>(
      method: M,
      ...[params]: BridgeEmpty extends BridgeParams<M>
        ? [params?: BridgeParams<M>]
        : [params: BridgeParams<M>]
    ): Promise<BridgeResult<M>> {
      const id = ++sequence;
      let timer: ReturnType<typeof setTimeout> | undefined;
      try {
        const result = await new Promise<BridgeResult<M>>((resolve, reject) => {
          waiting.set(id, {
            resolve(message) {
              if ("error" in message)
                reject(
                  Object.assign(new Error(message.error.message), {
                    data: message.error.data,
                  }),
                );
              // The decoded id selects this call's result contract.
              else resolve(message.result as BridgeResult<M>);
            },
            reject,
          });
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
          child.stdin?.write(JSON.stringify({ id, method, params }) + "\n");
        });
        return result;
      } finally {
        clearTimeout(timer);
        waiting.delete(id);
      }
    },
    answer<M extends BridgeServerMethod>(
      id: BridgeRpcId,
      result: BridgeServerResult<M>,
    ) {
      child.stdin?.write(JSON.stringify({ id, result }) + "\n");
    },
    note,
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
    child.stdin?.end();
    // A failed virtual-clock scenario can leave teardown waiting on a frozen timer.
    // Always reap this fixture's child, including when the test itself timed out.
    const guard = setTimeout(() => child.kill("SIGKILL"), HANG_GUARD_MS);
    try {
      // Bounded shutdown timers run on the virtual clock; advance it until exit.
      // A clock that can no longer be driven cannot finish the child: reap it.
      if (clock)
        while (!childExited())
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

export type BridgeHarness = Awaited<ReturnType<typeof startBridge>>;

// Resolves when the promise settles or after ms of real time.
async function waitReal(promise: Promise<void>, ms: number) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      promise,
      new Promise<void>((resolve) => (timer = setTimeout(resolve, ms))),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
