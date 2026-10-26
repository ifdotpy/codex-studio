import { present } from "./test-support.ts";
import assert from "node:assert/strict";
import { afterEach, test, vi } from "vitest";
import { EventEmitter } from "node:events";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRendererRecovery } from "./renderer-recovery.ts";
afterEach(() => vi.useRealTimers());

function fixture(
  options: Partial<Parameters<typeof createRendererRecovery>[0]> = {},
) {
  const profile = mkdtempSync(path.join(tmpdir(), "studio-renderer-contract-"));
  const workspaceURL = "http://127.0.0.1:4620/";
  let url = workspaceURL;
  const loads: string[] = [];
  const contents = Object.assign(new EventEmitter(), {
    isDestroyed: () => false,
    mainFrame: {},
    getOSProcessId: () => 42,
    isCrashed: () => false,
    getURL: () => url,
    loadURL: async (value: string) => {
      loads.push(value);
      url = value;
      contents.emit("did-finish-load");
    },
  });
  const win = Object.assign(new EventEmitter(), {
    webContents: contents,
    isDestroyed: () => false,
    setTitle: () => {},
  });
  const controller = createRendererRecovery({
    win,
    profile,
    workspaceURL,
    maxRetries: 0,
    ...options,
  });
  return {
    win,
    contents,
    loads,
    controller,
    workspaceURL,
    profile,
    close() {
      win.emit("close");
      win.emit("closed");
      rmSync(profile, { recursive: true, force: true });
    },
  };
}

test("an unexpected clean renderer exit reaches fallback; window close suppresses recovery", () => {
  const f = fixture();
  try {
    f.contents.emit(
      "render-process-gone",
      {},
      { reason: "clean-exit", exitCode: 0 },
    );
    assert.match(present(f.loads[0]), /^data:text\/html/);
    const records = readFileSync(
      path.join(f.profile, "renderer-recovery.jsonl"),
      "utf8",
    )
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    assert.equal(
      records.find((record) => record.event === "renderer-crash").reason,
      "clean-exit",
    );
    f.win.emit("close");
    f.contents.emit(
      "render-process-gone",
      {},
      { reason: "clean-exit", exitCode: 0 },
    );
    assert.equal(f.loads.length, 1);
  } finally {
    f.close();
  }
});

test("an initial load failure gives a reloadable fallback with no foreign-frame route", async () => {
  const f = fixture();
  try {
    const load = f.contents.loadURL;
    let fail = true;
    f.contents.loadURL = async (url) => {
      if (url === f.workspaceURL && fail) {
        fail = false;
        throw Object.assign(new Error("connection lost"), {
          code: "ERR_CONNECTION_RESET",
        });
      }
      return load(url);
    };
    assert.equal(await f.controller.reload({ manual: false }), true);
    assert.match(
      decodeURIComponent(present(f.loads[0])),
      /ERR_CONNECTION_RESET/,
    );
    const event = {
      preventDefault() {},
      url: f.workspaceURL,
      isMainFrame: true,
      frame: f.contents.mainFrame,
      initiator: {},
    };
    assert.equal(f.controller.handleNavigation(event), false);
    assert.equal(f.loads.length, 1);
    assert.equal(
      f.controller.handleNavigation({
        ...event,
        initiator: f.contents.mainFrame,
      }),
      true,
    );
    assert.equal(f.loads.at(-1), f.workspaceURL);
  } finally {
    f.close();
  }
});

test("only a continuously healthy renderer resets the automatic retry budget", async () => {
  vi.useFakeTimers();
  const f = fixture({ maxRetries: 1 });
  const crash = () =>
    f.contents.emit(
      "render-process-gone",
      {},
      { reason: "crashed", exitCode: 1 },
    );
  try {
    crash();
    await vi.advanceTimersByTimeAsync(1_000);
    assert.deepEqual(f.loads, [f.workspaceURL]);
    await vi.advanceTimersByTimeAsync(59_999);
    crash();
    assert.match(present(f.loads.at(-1)), /^data:text\/html/);
    await f.controller.reload();
    crash();
    await vi.advanceTimersByTimeAsync(1_000);
    await vi.advanceTimersByTimeAsync(60_000);
    const records = readFileSync(
      path.join(f.profile, "renderer-recovery.jsonl"),
      "utf8",
    )
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    assert.equal(
      records.find((record) => record.event === "renderer-healthy")
        .previousFailures,
      1,
    );
    crash();
    await vi.advanceTimersByTimeAsync(1_000);
    assert.equal(f.loads.at(-1), f.workspaceURL);
  } finally {
    f.close();
  }
});
