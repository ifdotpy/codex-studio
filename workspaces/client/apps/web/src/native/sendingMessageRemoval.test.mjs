import { expect, it, vi } from "vitest";
const ids = vi.hoisted(() => ({ next: 0 }));
vi.mock("../uuid", () => ({ randomUUID: () => `request:${++ids.next}` }));
import { createNativeSendingMessageRemoval } from "./useSendingMessageRemoval";
import { openNativePreferences } from "./preferenceStore";
import { createNativeLocalDrafts } from "./services/localDrafts";

function fixture() {
  const values = new Map();
  const calls = [];
  let lostCopy = true,
    lostCancellation = true;
  const storage = {
    get length() {
      return values.size;
    },
    key: (index) => [...values.keys()][index] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, raw) => values.set(key, raw),
    removeItem: (key) => values.delete(key),
    clear: () => values.clear(),
  };
  const services = {
    owner: { workspaceId: "a".repeat(32) },
    storage,
    async flush() {
      calls.push(["flush"]);
      if (lostCopy) {
        lostCopy = false;
        throw new Error("copy commit unknown");
      }
    },
    localDrafts: {
      async updateLocalDraft(key, fallback, update) {
        const next = update(
          JSON.parse(values.get(key) ?? JSON.stringify(fallback)),
        );
        if (next === null) values.delete(key);
        else values.set(key, JSON.stringify(next));
        return next;
      },
    },
    outbox: {
      async stopRemovedMessage(id) {
        calls.push(["stop", id]);
        return false;
      },
    },
    api: {
      async get(path) {
        calls.push(["get", path]);
        return {
          items: [{ id: "same", kind: "user", text: "original text" }],
          revision: "revision:1",
          capabilities: { receipts: true },
        };
      },
      async syncPost(path, body) {
        calls.push(["post", path, { ...body }]);
        if (lostCancellation) {
          lostCancellation = false;
          throw new Error("reply lost");
        }
        return {};
      },
    },
  };
  return {
    services,
    values,
    calls,
    removal: createNativeSendingMessageRemoval(services),
  };
}
const message = {
  id: "chat:same",
  clientMessageId: "same",
  role: "user",
  text: "original text",
  deliveryStatus: "sending",
};

it("commits the recoverable copy before cancellation and retries only the saved queue identity", async () => {
  const f = fixture();
  const scope = {
    stateDir: "state",
    workspaceId: f.services.owner.workspaceId,
  };
  const target = { chat: "chat", message };
  await expect(f.removal.removeSendingMessage(scope, target)).rejects.toThrow(
    "copy commit unknown",
  );
  expect(f.calls).toEqual([["flush"]]);
  await expect(f.removal.removeSendingMessage(scope, target)).rejects.toThrow(
    "reply lost",
  );
  const first = f.calls.find(([kind]) => kind === "post")[2];
  await f.removal.removeSendingMessage(scope, target);
  const posts = f.calls.filter(([kind]) => kind === "post");
  expect(posts).toHaveLength(2);
  expect(posts[1][2]).toEqual(first);
  expect(ids.next).toBe(1);
  expect(
    [...f.values.values()].some((raw) => raw.includes("original text")),
  ).toBe(true);
  expect(f.calls.some(([kind]) => kind === "enqueue")).toBe(false);
});

it("refuses another immutable workspace before storing or cancelling anything", async () => {
  const f = fixture();
  await expect(
    f.removal.removeSendingMessage(
      { stateDir: "state", workspaceId: "b".repeat(32) },
      { chat: "chat", message },
    ),
  ).rejects.toThrow(/workspace/i);
  expect(f.values.size).toBe(0);
  expect(f.calls).toEqual([]);
});

it("resumes an unknown final copy with the original CAS before the explicit removal retry", async () => {
  const raw = new Map(),
    receipts = new Map(),
    attempts = [];
  let next = 0,
    revision = 0,
    lost = true;
  const context = {
    async request(method, params, id) {
      if (method === "preferences.raw_all") return Object.fromEntries(raw);
      if (method === "system.uuid") return `cas:${++next}`;
      if (method === "preferences.raw_set") {
        raw.set(params.key, params.raw);
        return { saved: true, revision: String(++revision) };
      }
      if (method !== "preferences.raw_compare_exchange")
        throw new Error(method);
      attempts.push({ id, params: { ...params } });
      const previous = receipts.get(id);
      if (previous) {
        expect(previous.payload).toBe(JSON.stringify(params));
        return previous.reply;
      }
      const current = raw.get(params.key) ?? null;
      const applied = current === params.expectedRaw;
      if (applied) {
        if (params.raw === null) raw.delete(params.key);
        else raw.set(params.key, params.raw);
        revision++;
      }
      const reply = {
        applied,
        raw: applied ? params.raw : current,
        revision: String(revision),
      };
      receipts.set(id, { payload: JSON.stringify(params), reply });
      if (
        lost &&
        applied &&
        params.key.endsWith(":copies") &&
        params.raw.includes('"deliveryStatus":"cancelled"')
      ) {
        lost = false;
        throw new Error("final copy reply lost");
      }
      return reply;
    },
  };
  const storage = await openNativePreferences(context, vi.fn());
  const drafts = createNativeLocalDrafts(context, storage);
  const posts = [];
  const services = {
    owner: { workspaceId: "a".repeat(32) },
    storage,
    localDrafts: drafts,
    flush: () => drafts.flush(),
    outbox: { stopRemovedMessage: async () => false },
    api: {
      get: async () => ({
        items: [{ id: "same", kind: "user", text: "original text" }],
        revision: "revision:1",
        capabilities: { receipts: true },
      }),
      syncPost: async (_path, body) => {
        posts.push({ ...body });
        return {};
      },
    },
  };
  const removal = createNativeSendingMessageRemoval(services);
  const scope = { stateDir: "state", workspaceId: services.owner.workspaceId };
  await expect(
    removal.removeSendingMessage(scope, { chat: "chat", message }),
  ).rejects.toThrow("final copy reply lost");
  await removal.removeSendingMessage(scope, { chat: "chat", message });
  expect(posts).toHaveLength(2);
  expect(posts[1]).toEqual(posts[0]);
  const finalCopy = attempts.find(
    (item) =>
      item.params.key.endsWith(":copies") &&
      item.params.raw.includes('"deliveryStatus":"cancelled"'),
  );
  expect(attempts.filter((item) => item.id === finalCopy.id)).toHaveLength(2);
});
