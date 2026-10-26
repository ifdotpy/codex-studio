import { afterEach, test, vi } from "vitest";
import assert from "node:assert/strict";
import { fork, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { writeFileSync, type FSWatcher } from "node:fs";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { HANG_GUARD_MS, waitForFile } from "./testing/file-signals.ts";

// Lets a test run code when the helper starts a watcher, and replace the watcher with one that never reports events.
const watchHook = vi.hoisted(() => ({
  onWatch: undefined as undefined | (() => void),
}));
vi.mock("node:fs", async (importOriginal) => {
  const actual = await importOriginal<typeof import("node:fs")>();
  return {
    ...actual,
    watch: (...args: Parameters<typeof actual.watch>) => {
      if (!watchHook.onWatch) return actual.watch(...args);
      watchHook.onWatch();
      return { close() {}, on() {} } as unknown as FSWatcher;
    },
  };
});

afterEach(() => {
  watchHook.onWatch = undefined;
});

test("a file created after the watch starts is found without a watch event", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-file-signals-"));
  try {
    const file = path.join(root, "signal");
    // The file appears in the gap between starting the watch and the first check.
    watchHook.onWatch = () => writeFileSync(file, "");
    await waitForFile(file);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("a file whose watch event is missed is found by the bounded re-check", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-file-signals-"));
  try {
    const file = path.join(root, "signal");
    watchHook.onWatch = () => {};
    const pending = waitForFile(file);
    await fs.writeFile(file, "");
    await pending;
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("test-side file waits preserve IPC while awaiting a delayed parent reply", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-file-signals-"));
  let child: ChildProcess | undefined;
  let replyTimer: ReturnType<typeof setTimeout> | undefined;
  try {
    const file = path.join(root, "ready");
    await fs.writeFile(file, "ready");
    child = fork(
      new URL("./file-signals.fixture.mts", import.meta.url),
      [file],
      {
        stdio: ["ignore", "ignore", "pipe", "ipc"],
        execArgv: [],
      },
    );
    const messages: unknown[] = [];
    let stderr = "";
    child.stderr?.on("data", (chunk) => (stderr += chunk));
    child.on("message", (message) => {
      messages.push(message);
      if (message === "ready") {
        // Leave the child waiting only on its IPC channel, as a Vitest worker
        // does while awaiting an RPC reply from the test runner.
        replyTimer = setTimeout(() => {
          if (child?.connected) child.send("replied", () => {});
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
