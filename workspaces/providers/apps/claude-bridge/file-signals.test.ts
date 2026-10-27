import { test, vi } from "vitest";
import assert from "node:assert/strict";
import { fork, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { existsSync, watch } from "node:fs";
import { mock } from "node:test";
import { HANG_GUARD_MS, waitForFile } from "./testing/file-signals.ts";

vi.mock("node:fs", async (importOriginal) => {
  const original = await importOriginal<typeof import("node:fs")>();
  return {
    ...original,
    existsSync: vi.fn(original.existsSync),
    watch: vi.fn(original.watch),
  };
});

test.each([false, true])(
  "file waits detect creation before the first stat sample without a watch event (frozen clock: %s)",
  async (frozenClock) => {
    const root = await fs.mkdtemp(
      path.join(os.tmpdir(), "claude-file-signals-"),
    );
    const controller = new AbortController();
    try {
      const file = path.join(root, ".registered");
      await fs.writeFile(file, "");
      const original =
        await vi.importActual<typeof import("node:fs")>("node:fs");
      vi.mocked(watch).mockImplementation((...args) => {
        const watcher = original.watch(...args);
        watcher.removeAllListeners("change");
        return watcher;
      });
      // The synchronous check sees absence, but the asynchronous stat starts with
      // the file present. Neither watcher reports a later change in this ordering.
      vi.mocked(existsSync).mockReturnValueOnce(false);
      if (frozenClock)
        mock.timers.enable({ apis: ["Date", "setTimeout", "setInterval"] });
      await waitForFile(file, {
        signal: AbortSignal.any([
          controller.signal,
          AbortSignal.timeout(HANG_GUARD_MS),
        ]),
      });
      assert.equal(existsSync(file), true);
    } finally {
      controller.abort();
      if (frozenClock) mock.timers.reset();
      vi.mocked(existsSync).mockReset();
      vi.mocked(watch).mockReset();
      await fs.rm(root, { recursive: true, force: true });
    }
  },
);

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
