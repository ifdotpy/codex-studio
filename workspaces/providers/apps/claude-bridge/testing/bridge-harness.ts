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
function parseBridgeMessage<M extends BridgeMethod>(
  value: unknown,
): BridgeNote | BridgeResponse<BridgeResult<M>> {
  if (!isRecord(value)) throw new Error("Invalid bridge message envelope");
  const validId = typeof value.id === "string" || typeof value.id === "number";
  if (typeof value.method === "string") {
    if (!("params" in value) || (value.id !== undefined && !validId))
      throw new Error("Invalid bridge notification envelope");
  } else if (
    !validId ||
    (!("result" in value) &&
      !(
        isRecord(value.error) &&
        typeof value.error.message === "string" &&
        typeof value.error.code === "number"
      ))
  ) {
    throw new Error("Invalid bridge response envelope");
  }
  // The child is the copied program under test and its scripted SDK fixture.
  // Their payloads use the TypeScript protocol owner; this boundary checks the
  // envelope and trusts those controlled producers for the payload fields.
  return value as BridgeNote | BridgeResponse<BridgeResult<M>>;
}

// Maximum simulated wait and per-step advancement, both in milliseconds.
const VIRTUAL_NOTIFICATION_BUDGET_MS = 10_000;
const VIRTUAL_STEP_MS = 10;
const BRIDGE = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
import { HANG_GUARD_MS } from "./file-signals.ts";
export { HANG_GUARD_MS };

// Copies the bridge sources to a temporary folder, replaces the SDK import with
// `fakeSdk` (module source text) and starts the bridge with one new thread.
export async function startBridge(
  fakeSdk: string | URL,
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
  // SDK init always carries a native session id. Keep older scripted fixtures
  // faithful to that declaration; an explicit session_id still takes precedence.
  fakeSdk = (
    fakeSdk instanceof URL ? await fs.readFile(fakeSdk, "utf8") : fakeSdk
  ).replace(
    /subtype:\s*(["'])init\1/g,
    "subtype:'init',session_id:options.resume||options.sessionId",
  );
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(temporaryBase, prefix)),
  );
  for (const name of await fs.readdir(BRIDGE)) {
    if (!/\.(mjs|mts|ts)$/.test(name) || /\.test\.(mjs|mts|ts)$/.test(name))
      continue;
    const source = await fs.readFile(path.join(BRIDGE, name), "utf8");
    await fs.writeFile(
      path.join(root, name),
      source.replaceAll(
        "@anthropic-ai/claude-agent-sdk",
        /^history-index\.(mjs|ts)$/.test(name)
          ? path.join(
              BRIDGE,
              "node_modules/@anthropic-ai/claude-agent-sdk/sdk.mjs",
            )
          : "./fake.mts",
      ),
    );
  }
  await fs.writeFile(
    path.join(root, "fake.mts"),
    `import { waitForFile as waitForFileSignal } from ${JSON.stringify(path.join(BRIDGE, "testing/file-signals.ts"))};\n` +
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
  let stderr = "";
  const notes: BridgeNote[] = [];
  const waiting = new Map<
    BridgeRpcId,
    { resolve: (value: unknown) => void; reject: (error: Error) => void }
  >();
  const notifications = new Set<() => void>();
  const attach = () => {
    child = launch();
    const attached = child;
    exited = new Promise<void>((resolve) =>
      attached.once("exit", () => resolve()),
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
        else waiting.get(message.id)?.resolve(value);
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
    predicate: (note: BridgeNote) => boolean,
  ): Promise<BridgeNote>;
  function advanceUntil<M extends BridgeNote["method"]>(
    method: M,
    predicate?: (note: BridgeNoteFor<M>) => boolean,
  ): Promise<BridgeNoteFor<M>>;
  async function advanceUntil<M extends BridgeNote["method"]>(
    methodOrPredicate: M | ((note: BridgeNote) => boolean),
    filter?: (note: BridgeNoteFor<M>) => boolean,
  ): Promise<BridgeNote> {
    const predicate =
      typeof methodOrPredicate === "function"
        ? methodOrPredicate
        : (entry: BridgeNote) =>
            hasMethod(entry, methodOrPredicate) && (!filter || filter(entry));
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
  }
  function note<N extends BridgeNote>(
    predicate: (note: BridgeNote) => note is N,
  ): Promise<N>;
  function note(predicate: (note: BridgeNote) => boolean): Promise<BridgeNote>;
  function note<M extends BridgeNote["method"]>(
    method: M,
    predicate?: (note: BridgeNoteFor<M>) => boolean,
  ): Promise<BridgeNoteFor<M>>;
  async function note<M extends BridgeNote["method"]>(
    methodOrPredicate: M | ((note: BridgeNote) => boolean),
    filter?: (note: BridgeNoteFor<M>) => boolean,
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
            resolve(value) {
              const message = parseBridgeMessage<M>(value);
              if ("method" in message) return;
              if ("error" in message)
                reject(
                  Object.assign(new Error(message.error.message), {
                    data: message.error.data,
                  }),
                );
              else resolve(message.result);
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

export type BridgeHarness = Awaited<ReturnType<typeof startBridge>>;
