import { afterEach, expect, it, vi } from "vitest";
vi.mock("../uuid", () => {
  let next = 0;
  return { randomUUID: () => `fixture-${++next}` };
});
vi.mock("../servers/environment", () => ({
  serverStorageName: (name) => name,
}));
vi.mock("../servers/storage", () => ({ serverLocalStorage: {} }));
vi.mock("../api", () => ({
  syncGet: vi.fn(),
  syncPost: vi.fn(),
  refreshSession: vi.fn(),
}));
vi.mock("./localDraft", () => ({ writeLocalDraft: vi.fn() }));
vi.mock("./client", () => ({ syncDatabase: vi.fn() }));
import { createUploads } from "./uploads";

// A bounded transaction fixture supplies storage mechanics only. The production
// queue owner decides which identities and bytes can replay or enter a commit.
function databaseFixture() {
  const tables = new Map([
      ["uploads", new Map()],
      ["contents", new Map()],
    ]),
    writes = [];
  const database = {
    close() {},
    transaction(names, mode, options) {
      if (mode === "readwrite")
        expect(options).toEqual({ durability: "strict" });
      const working = new Map(
        names.map((name) => [
          name,
          new Map(structuredClone([...tables.get(name)])),
        ]),
      );
      let pending = 0,
        aborted = false,
        finished = false;
      const tx = {
        error: null,
        objectStore(name) {
          const rows = working.get(name);
          if (!rows) throw Error("Unexpected object store");
          const request = (operation) => {
            const result = { result: undefined };
            pending++;
            queueMicrotask(() => {
              if (aborted) return;
              try {
                result.result = structuredClone(operation());
                result.onsuccess?.({ target: result });
              } catch (error) {
                tx.error = error;
                tx.abort();
                return;
              }
              pending--;
              queueMicrotask(() => {
                if (pending || aborted || finished) return;
                finished = true;
                if (mode === "readwrite")
                  for (const [key, values] of working) tables.set(key, values);
                tx.oncomplete?.();
              });
            });
            return result;
          };
          return {
            get: (id) => request(() => rows.get(id)),
            getAll: () => request(() => [...rows.values()]),
            count: () => request(() => rows.size),
            add: (row) =>
              request(() => {
                if (rows.has(row.id)) throw Error("Duplicate key");
                writes.push({ table: name, id: row.id });
                rows.set(row.id, structuredClone(row));
                return row.id;
              }),
          };
        },
        abort() {
          if (aborted || finished) return;
          aborted = true;
          queueMicrotask(() => tx.onabort?.({ target: tx }));
        },
      };
      return tx;
    },
  };
  return { database, tables, writes };
}
afterEach(() => vi.unstubAllGlobals());
it("the browser upload owner replays a lost committed enqueue and preflights the entire batch before writes", async () => {
  const fixture = databaseFixture(),
    workspace = "a".repeat(32),
    storage = new Map();
  const dispatchEvent = vi.fn(() => true);
  vi.stubGlobal("window", { dispatchEvent });
  vi.stubGlobal("BroadcastChannel", undefined);
  const forbidden = () => {
    throw Error("No network operation is allowed");
  };
  const uploads = createUploads(
    {
      databaseName: "isolated-upload-replay",
      storage: { getItem: (key) => storage.get(key) ?? null },
      syncGet: forbidden,
      syncPost: forbidden,
      refreshSession: forbidden,
      writeLocalDraft: forbidden,
      syncDatabase: async () => ({ workspaceId: workspace }),
    },
    async () => fixture.database,
  );
  const original = [{ id: "original-upload", created: 123 }],
    file = new File(["body"], "note.txt", { type: "text/plain" });
  dispatchEvent.mockImplementationOnce(() => {
    throw Error("Lost response after commit");
  });
  await expect(
    uploads.queueUploads("project", "agent", [file], workspace, original),
  ).rejects.toThrow("Lost response after commit");
  const before = await uploads.pendingUploads("project", workspace);
  expect(before).toHaveLength(1);
  const replay = await uploads.queueUploads(
    "project",
    "agent",
    [file],
    workspace,
    original,
  );
  expect(await uploads.pendingUploads("project", workspace)).toEqual(before);
  expect(replay).toEqual(before);
  expect(before[0]).toMatchObject({ id: "original-upload", created: 123 });
  expect(fixture.writes).toHaveLength(2);
  expect(
    new Uint8Array(fixture.tables.get("contents").get("original-upload").bytes),
  ).toEqual(new TextEncoder().encode("body"));
  await expect(
    uploads.queueUploads("project", "other-agent", [file], workspace, original),
  ).rejects.toThrow("different contents or another owner");
  await expect(
    uploads.queueUploads(
      "project",
      "agent",
      [file, new File(["evil"], "note.txt", { type: "text/plain" })],
      workspace,
      [{ id: "new-upload", created: 124 }, ...original],
    ),
  ).rejects.toThrow("different contents or another owner");
  expect(fixture.writes).toHaveLength(2);
  expect(await uploads.pendingUploads("project", workspace)).toEqual(before);
});
