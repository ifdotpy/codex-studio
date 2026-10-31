import { expect, it } from "vitest";
import {
  cleanupLegacyPersistence,
  outboxRequestKeys,
  outboxRequests,
} from "./browserPersistence";

it("keeps typed outbox operations at their existing keys", () => {
  const values = new Map<string, string>();
  const storage = {
    get length() {
      return values.size;
    },
    key: (index: number) => [...values.keys()][index] ?? null,
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
  } as Storage;
  const outbox = outboxRequests(storage);
  outbox.set("nativeAction", "workspace:agent", "body");
  expect(values.get("studio-native-action:workspace:agent")).toBe("body");
  expect(outbox.get("nativeAction", "workspace:agent")).toBe("body");
  expect(outboxRequestKeys(storage, "nativeAction", "workspace:")).toEqual([
    "workspace:agent",
  ]);
  outbox.remove("nativeAction", "workspace:agent");
  expect(values.size).toBe(0);
});

it("removes retired draft and upload data without touching identities", async () => {
  const values = new Map([
    ["codex-chat-draft:workspace:chat", "draft"],
    [":server:remote:studio-session-cost:session", "cost"],
    ["studio-upload-cancelled:one", "true"],
    ["studio-sidebar-width:scope", "20"],
    ["codex-sidebar-order:scope", "cached"],
    ["codex-sidebar-order:scope:pending", "pending"],
    ["studio-removed-messages:chat", "old"],
    ["codex-queue-edit:scope:writer", "old"],
    [
      "codex-pending-creation:scope:rejected:00000000-0000-4000-8000-000000000000",
      "old",
    ],
    [
      "codex-pending-creation:scope:dismissed:00000000-0000-4000-8000-000000000000",
      "old",
    ],
    ["codex-pending-creation:scope:request:active", "request"],
    ["studio-queue-change:scope:chat", "request"],
    ["studio-paired-servers-v1", "servers"],
    ["unrelated-app-key", "other"],
  ]);
  const storage = {
    get length() {
      return values.size;
    },
    key: (index: number) => [...values.keys()][index] ?? null,
    removeItem: (key: string) => values.delete(key),
  };
  const deleted: string[] = [];
  const factory = {
    databases: async () => [
      { name: "codex-studio-dictation" },
      { name: "codex-studio-uploads:server:remote" },
      { name: "studio-device-key-v1" },
      { name: "studio-monitor-input-journal-v1" },
    ],
    deleteDatabase: (name: string) => {
      deleted.push(name);
      const request = {} as IDBOpenDBRequest;
      queueMicrotask(() => request.onsuccess?.(new Event("success")));
      return request;
    },
  };
  await cleanupLegacyPersistence(storage, factory);
  expect([...values.keys()]).toEqual([
    "codex-sidebar-order:scope:pending",
    "codex-pending-creation:scope:request:active",
    "studio-queue-change:scope:chat",
    "studio-paired-servers-v1",
    "unrelated-app-key",
  ]);
  expect(deleted).toEqual([
    "codex-studio-dictation",
    "codex-studio-uploads:server:remote",
  ]);
});
