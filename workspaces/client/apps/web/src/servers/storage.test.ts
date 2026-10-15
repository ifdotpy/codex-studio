import { createChatCreationStore } from "../chatCreation";
import { createLocalDraftStorage } from "../sync/localDraft";
import { expect, it } from "vitest";
import {
  scopedStorage,
  serverStoragePrefix,
  createStorageEventKey,
} from "./storage";
function memoryStorage(): Storage {
  const rows = new Map<string, string>();
  return {
    get length() {
      return rows.size;
    },
    key: (i) => [...rows.keys()][i] ?? null,
    getItem: (key) => rows.get(key) ?? null,
    setItem: (key, value) => {
      rows.set(key, value);
    },
    removeItem: (key) => {
      rows.delete(key);
    },
    clear: () => rows.clear(),
  };
}
it("isolates all keys, enumeration and deletion with identical draft and state paths", () => {
  const raw = memoryStorage();
  raw.setItem("studio-preferences", "old");
  const first = scopedStorage(() => raw, "server:first:");
  const second = scopedStorage(() => raw, "server:second:");
  first.setItem("draft:/same/path:chat", "first draft");
  second.setItem("draft:/same/path:chat", "second draft");
  expect(first.getItem("draft:/same/path:chat")).toBe("first draft");
  expect(second.getItem("draft:/same/path:chat")).toBe("second draft");
  expect(first.length).toBe(1);
  expect(first.key(0)).toBe("draft:/same/path:chat");
  expect(first.key(1)).toBeNull();
  first.clear();
  expect(first.length).toBe(0);
  expect(second.length).toBe(1);
  expect(raw.getItem("studio-preferences")).toBe("old");
});
it("preserves the local installation's existing storage identity", () => {
  const raw = memoryStorage();
  raw.setItem("codex-sync-workspace", "cached-workspace");
  const local = scopedStorage(() => raw, "");
  expect(local.getItem("codex-sync-workspace")).toBe("cached-workspace");
  local.setItem("draft", "text");
  expect(raw.getItem("draft")).toBe("text");
});

it("keeps requests and synchronous drafts with their owner despite matching keys and request IDs", () => {
  const raw = memoryStorage();
  const storage = (owner: string) =>
    scopedStorage(() => raw, serverStoragePrefix(owner));
  const a = createChatCreationStore(storage("a"));
  const b = createChatCreationStore(storage("b"));
  a.saveChatCreation("same-key", { id: "same-request", project: "a" });
  b.saveChatCreation("same-key", { id: "same-request", project: "b" });
  b.confirmChatCreation("same-key", "same-request");
  expect(a.pendingChatCreations("same-key")).toEqual([
    { id: "same-request", project: "a" },
  ]);
  expect(b.pendingChatCreations("same-key")).toEqual([]);
  const draftsA = createLocalDraftStorage(storage("a"), "db-a");
  const draftsB = createLocalDraftStorage(storage("b"), "db-b");
  draftsA.writeLocalDraft("same-key", { request_id: "a" });
  draftsB.writeLocalDraft("same-key", { request_id: "b" });
  draftsB.writeLocalDraft("same-key", null);
  expect(storage("a").getItem("same-key")).toBe('{"request_id":"a"}');
  expect(storage("b").getItem("same-key")).toBeNull();
});

it("routes storage events only to the matching server namespace", () => {
  const a = createStorageEventKey(serverStoragePrefix("a"));
  const b = createStorageEventKey(serverStoragePrefix("b"));
  const event = { key: serverStoragePrefix("a") + "request" };
  expect(a(event)).toBe("request");
  expect(b(event)).toBeUndefined();
  expect(a({ key: null })).toBeNull();
  expect(serverStoragePrefix("local")).toBe("");
});
