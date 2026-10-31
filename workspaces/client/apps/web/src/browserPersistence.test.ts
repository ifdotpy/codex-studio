import { expect, it } from "vitest";
import { cleanupLegacyPersistence } from "./browserPersistence";

it("removes retired draft and upload data without touching identities", async () => {
  const values = new Map([
    ["codex-chat-draft:workspace:chat", "draft"],
    [":server:remote:studio-session-cost:session", "cost"],
    ["studio-upload-cancelled:one", "true"],
    ["studio-sidebar-width:scope", "20"],
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
  } as unknown as Storage;
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
  } as unknown as IDBFactory;
  await cleanupLegacyPersistence(storage, factory);
  expect([...values.keys()]).toEqual([
    "studio-queue-change:scope:chat",
    "studio-paired-servers-v1",
    "unrelated-app-key",
  ]);
  expect(deleted).toEqual([
    "codex-studio-dictation",
    "codex-studio-uploads:server:remote",
  ]);
});
