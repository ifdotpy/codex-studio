import { assert } from "vitest";
import { afterEach, expect, it, vi } from "vitest";
import { importLegacyDrafts, readLegacyDrafts } from "./legacyDrafts";

const workspace = "a".repeat(32);
const scope = `codex-drafts:${workspace}`;
function fixture(values: Record<string, string>, rows: unknown[] = []) {
  const data = new Map(Object.entries(values));
  vi.stubGlobal("localStorage", {
    get length() {
      return data.size;
    },
    key: (index: number) => [...data.keys()][index] ?? null,
    getItem: (key: string) => data.get(key) ?? null,
  });
  const close = vi.fn();
  vi.stubGlobal("indexedDB", {
    databases: async () =>
      rows.length
        ? [{ name: `rxdb-dexie-studio${workspace}--0--drafts`, version: 10 }]
        : [],
    open: vi.fn(() => {
      const request = {
        result: {
          close,
          transaction: vi.fn((name: string, mode: string) => {
            expect([name, mode]).toEqual(["docs", "readonly"]);
            const tx = {
              oncomplete: undefined as undefined | (() => void),
              objectStore: () => ({ getAll: () => ({ result: rows }) }),
            };
            queueMicrotask(() => tx.oncomplete?.());
            return tx;
          }),
        },
        onsuccess: undefined as undefined | (() => void),
      };
      queueMicrotask(() => request.onsuccess?.());
      return request;
    }),
  });
  return { data, close };
}
afterEach(() => vi.unstubAllGlobals());
it("reads stored branches and pending edits without replication and reuses the exact import identity", async () => {
  const branch = {
    id: "device:writer:room",
    device: "device",
    session: "room",
    text: "saved",
    updated: 1,
  };
  const newer = { ...branch, text: "unsent Ж🦀", updated: 2 };
  const { close, data } = fixture(
    { [`${scope}:pending:writer:room`]: JSON.stringify(newer) },
    [{ id: branch.id, payload: JSON.stringify(branch), _deleted: 0 }],
  );
  expect(await readLegacyDrafts(workspace)).toEqual([branch, newer]);
  expect(close).toHaveBeenCalledOnce();
  const invoke = vi
    .fn()
    .mockRejectedValueOnce(new Error("lost reply"))
    .mockResolvedValue({});
  await expect(importLegacyDrafts(workspace, invoke)).rejects.toThrow(
    "lost reply",
  );
  await importLegacyDrafts(workspace, invoke);
  expect(invoke.mock.calls[0]).toEqual(invoke.mock.calls[1]);
  assert(invoke.mock.calls[0] !== undefined);
  expect(invoke.mock.calls[0][0]).toBe("drafts.import");
  expect(data.size).toBe(1);
});
it("does not resurrect an old snapshot after a per-session deletion", async () => {
  fixture({
    [scope]: JSON.stringify({ room: "stale" }),
    [`codex-chat-draft:${workspace}:room`]: JSON.stringify({
      version: 1,
      workspace,
      session: "room",
      text: "",
      deleted: true,
      source: "local",
      updated: 8,
    }),
  });
  expect(await readLegacyDrafts(workspace)).toEqual([
    {
      id: "browser-import:room",
      device: "browser-import",
      session: "room",
      text: "",
      updated: 8,
    },
  ]);
});
it("stops before native mutation when saved data is corrupt", async () => {
  fixture({ [`${scope}:pending:writer:room`]: "broken" });
  const invoke = vi.fn();
  await expect(importLegacyDrafts(workspace, invoke)).rejects.toThrow();
  expect(invoke).not.toHaveBeenCalled();
});
