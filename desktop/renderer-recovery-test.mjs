import assert from "node:assert/strict";
import { test } from "node:test";
import { EventEmitter } from "node:events";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
const { createRendererRecovery } = createRequire(import.meta.url)(
  "./renderer-recovery.cjs",
);
function fixture() {
  const profile = mkdtempSync(path.join(tmpdir(), "studio-renderer-contract-"));
  const workspaceURL = "http://127.0.0.1:4620/";
  const win = new EventEmitter();
  const contents = new EventEmitter();
  win.webContents = contents;
  win.isDestroyed = () => false;
  win.setTitle = () => {};
  contents.isDestroyed = () => false;
  contents.mainFrame = {};
  contents.getOSProcessId = () => 42;
  contents.isCrashed = () => false;
  let url = workspaceURL;
  const loads = [];
  contents.getURL = () => url;
  contents.loadURL = async (value) => {
    loads.push(value);
    url = value;
    contents.emit("did-finish-load");
  };
  const controller = createRendererRecovery({
    win,
    profile,
    workspaceURL,
    maxRetries: 0,
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
    assert.match(f.loads[0], /^data:text\/html/);
    const records = readFileSync(
      path.join(f.profile, "renderer-recovery.jsonl"),
      "utf8",
    )
      .trim()
      .split("\n")
      .map(JSON.parse);
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
    assert.match(decodeURIComponent(f.loads[0]), /ERR_CONNECTION_RESET/);
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
