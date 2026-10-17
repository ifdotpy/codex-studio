import { afterEach, describe, expect, it, vi } from "vitest";
import { createApiScope, ApiSchemaMismatchError } from "../api";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";
import { createMutationJournal } from "./mutationIdentity";
import { scopedStorage } from "./storage";

function memory(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    key: (i) => [...values.keys()][i] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      values.set(key, value);
    },
    removeItem: (key) => {
      values.delete(key);
    },
    clear: () => values.clear(),
  };
}
function scope(
  id: string,
  fetch: (request: Request) => Promise<Response>,
  disk: Storage,
) {
  const storage = scopedStorage(() => disk, id + ":");
  return createApiScope({
    origin: `https://${id}.example`,
    fetch,
    localStorage: storage,
    sessionStorage: storage,
    beginMutation: createMutationJournal(id, storage),
    beginActivity: () => () => {},
    entityCheckpoint: () => undefined,
  });
}
const response = (value: unknown, hash = API_SCHEMA_HASH) =>
  new Response(JSON.stringify(value), {
    headers: {
      [API_SCHEMA_HASH_HEADER]: hash,
      "Content-Type": "application/json",
    },
  });

describe("concurrent servers in one interface", () => {
  it("persists changed UI values once per owner and observes intervening storage edits", () => {
    const disk = memory();
    const write = vi.spyOn(disk, "setItem");
    const a = scope("a", async () => response({}), disk);
    const b = scope("b", async () => response({}), disk);
    const position = { top: 120, following: false, distance: 240 };
    a.save("studio-chat-scroll:chat", position);
    a.save("studio-chat-scroll:chat", { ...position });
    b.save("studio-chat-scroll:chat", position);
    expect(write).toHaveBeenCalledTimes(2);
    a.save("studio-chat-scroll:chat", { ...position, top: 121 });
    expect(write).toHaveBeenCalledTimes(3);
    disk.setItem("a:studio-chat-scroll:chat", "null");
    a.save("studio-chat-scroll:chat", { ...position, top: 121 });
    expect(write).toHaveBeenCalledTimes(5);
    expect(b.saved("studio-chat-scroll:chat", null)).toEqual(position);
  });

  afterEach(() => vi.unstubAllGlobals());
  it("keeps tokens, workspaces and late mutation persistence with their owner", async () => {
    vi.stubGlobal("window", new EventTarget());
    const disk = memory();
    const requests: Request[] = [];
    let complete!: (response: Response) => void;
    const a = scope(
      "a",
      async (request) => {
        requests.push(request);
        return new Promise((resolve) => {
          complete = resolve;
        });
      },
      disk,
    );
    const b = scope(
      "b",
      async (request) => {
        requests.push(request);
        return response({ ok: true });
      },
      disk,
    );
    a.setToken("a-token");
    a.setWorkspace("a-workspace");
    b.setToken("b-token");
    b.setWorkspace("b-workspace");
    const persisted: string[] = [];
    a.registerSyncEntityPersister(async (workspace) => {
      persisted.push("a:" + workspace);
    });
    b.registerSyncEntityPersister(async (workspace) => {
      persisted.push("b:" + workspace);
    });
    const first = a.post(
      "/api/rename",
      { id: "same-id", name: "A" },
      { requestId: "write-a" },
    );
    await b.post(
      "/api/rename",
      { id: "same-id", name: "B" },
      { requestId: "write-b" },
    );
    complete(response({ _syncEntities: [{ id: "entity" }] }));
    await first;
    expect(
      requests.map((r) => [
        new URL(r.url).hostname,
        r.headers.get("X-Canvas-Token"),
        r.headers.get("X-Canvas-Workspace"),
        r.headers.get("X-Studio-Request-Id"),
      ]),
    ).toEqual([
      ["a.example", "a-token", "a-workspace", "write-a"],
      ["b.example", "b-token", "b-workspace", "write-b"],
    ]);
    expect(persisted).toEqual(["a:a-workspace"]);
    a.save("draft", "A");
    b.save("draft", "B");
    expect(a.saved("draft", "")).toBe("A");
    expect(b.saved("draft", "")).toBe("B");
  });
  it("a schema mismatch only stops the affected server", async () => {
    const disk = memory();
    const a = scope("a", async () => response({}, "0".repeat(64)), disk);
    const b = scope("b", async () => response({ token: "b-token" }), disk);
    await expect(a.refreshSession()).rejects.toBeInstanceOf(
      ApiSchemaMismatchError,
    );
    await expect(b.refreshSession()).resolves.toMatchObject({
      token: "b-token",
    });
    expect(a.isApiSchemaMismatch()).toBe(true);
    expect(b.isApiSchemaMismatch()).toBe(false);
  });
});

it("separates transcript and sequence caches even when server object IDs collide", async () => {
  const { createTranscriptCache } = await import("../sync/transcriptCache");
  const { createEntitySequenceScope } = await import("../sync/entitySequence");
  const a = createTranscriptCache();
  const b = createTranscriptCache();
  a.cacheTranscriptValue(
    "same-workspace",
    "same-chat",
    { items: [{ id: "same-message", text: "A" }] },
    10,
  );
  b.cacheTranscriptValue(
    "same-workspace",
    "same-chat",
    { items: [{ id: "same-message", text: "B" }] },
    1,
  );
  expect(
    a.peekTranscript("same-workspace", "same-chat")?.payload.items[0].text,
  ).toBe("A");
  expect(
    b.peekTranscript("same-workspace", "same-chat")?.payload.items[0].text,
  ).toBe("B");
  a.clearTranscript("same-workspace", "same-chat");
  expect(b.peekTranscript("same-workspace", "same-chat")?.seq).toBe(1);
  const first = createEntitySequenceScope().setEntitySequenceProjection(
    "same-workspace",
    API_SCHEMA_HASH,
  );
  const second = createEntitySequenceScope().setEntitySequenceProjection(
    "same-workspace",
    API_SCHEMA_HASH,
  );
  first.assign(100);
  second.assign(1);
  first.reset();
  expect(second.value).toBe(1);
});

it("keeps draft records and interrupted draft journals with their server", async () => {
  const { createDraftStorage } = await import("../sync/draftStorage");
  const { createDraftJournal } = await import("../sync/draftJournal");
  const disk = memory();
  const storageA = scopedStorage(() => disk, "a:");
  const storageB = scopedStorage(() => disk, "b:");
  const a = createDraftStorage(storageA),
    b = createDraftStorage(storageB);
  const journalA = createDraftJournal(storageA),
    journalB = createDraftJournal(storageB);
  const scope = "codex-drafts:identical-workspace";
  a.writeLocalDraftRecord(scope, "identical-chat", "A", "local");
  b.writeLocalDraftRecord(scope, "identical-chat", "B", "local");
  const entry = {
    key: scope + ":pending:edit",
    version: {
      id: "device:identical-chat",
      session: "identical-chat",
      device: "device",
      text: "pending A",
      updated: 1,
    },
  };
  journalA.writeDraftJournal(entry);
  expect(journalA.readDraftJournal(scope)).toEqual([entry]);
  expect(journalB.readDraftJournal(scope)).toEqual([]);
  expect(a.readLocalDraftRecord(scope, "identical-chat")?.text).toBe("A");
  expect(b.readLocalDraftRecord(scope, "identical-chat")?.text).toBe("B");
  journalB.forgetDraftJournal(entry);
  expect(journalA.readDraftJournal(scope)).toEqual([entry]);
});
