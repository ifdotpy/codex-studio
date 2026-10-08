import { test, expect } from "vitest";
import vm from "node:vm";
import { EventEmitter } from "node:events";
import { readFile } from "node:fs/promises";
test("preload releases stream and progress subscribers when its frame leaves", async () => {
  const ipc = new EventEmitter();
  const events = {};
  let bridge;
  vm.runInNewContext(
    await readFile(new URL("./preload.cjs", import.meta.url), "utf8"),
    {
      process: { isMainFrame: true, platform: "darwin", argv: [] },
      URLSearchParams,
      location: { search: "" },
      window: {
        addEventListener: (name, listener) => (events[name] = listener),
      },
      performance: { now: () => 0 },
      require: () => ({
        ipcRenderer: ipc,
        contextBridge: {
          exposeInMainWorld: (_name, value) => (bridge = value),
        },
      }),
    },
  );
  bridge.onServerStream(() => {});
  bridge.onTranscriptionProgress(() => {});
  expect(ipc.listenerCount("codex-desktop-server-stream")).toBe(1);
  expect(ipc.listenerCount("codex-desktop-transcription-progress")).toBe(1);
  events.pagehide?.();
  expect(ipc.listenerCount("codex-desktop-server-stream")).toBe(0);
  expect(ipc.listenerCount("codex-desktop-transcription-progress")).toBe(0);
});
