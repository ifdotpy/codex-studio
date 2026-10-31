/** Browser databases allowed for device keys, credentials, and request fences. */
export const browserDatabases = {
  deviceKey: "studio-device-key-v1",
  serverCredentials: "studio-server-credentials-v1",
  monitorInput: "studio-monitor-input-journal-v1",
  outbox: "studio-thin-cache",
} as const;

export const browserStores = {
  deviceKey: "key",
  serverCredentials: "credentials",
  monitorInput: "inputs",
  outbox: "unsent",
} as const;

export function browserIndexedDB(): IDBFactory | undefined {
  return globalThis.indexedDB;
}

const outboxPrefixes = {
  nativeAction: "studio-native-action:",
  projectSave: "studio-peer-convert:",
  queueChange: "studio-queue-change:",
  serverAccess: "studio-server-access-pending",
  pairing: "studio-pairing-attempt-v1",
  serverManagement: "studio-server-management-v1:",
  legacyServerManagement: "studio-server-discovery-pending-v1",
} as const;
export type OutboxRequestKind = keyof typeof outboxPrefixes;

/** One logical outbox, retaining existing keys and exact request bytes. */
export function outboxRequests(
  storage: Pick<
    Storage,
    "getItem" | "setItem" | "removeItem"
  > = globalThis.localStorage,
) {
  const key = (kind: OutboxRequestKind, suffix: string) =>
    outboxPrefixes[kind] + suffix;
  return {
    key,
    get: (kind: OutboxRequestKind, suffix: string) =>
      storage.getItem(key(kind, suffix)),
    set: (kind: OutboxRequestKind, suffix: string, value: string) =>
      storage.setItem(key(kind, suffix), value),
    remove: (kind: OutboxRequestKind, suffix: string) =>
      storage.removeItem(key(kind, suffix)),
  };
}

export function outboxRequestKeys(
  storage: Storage,
  kind: OutboxRequestKind,
  suffix: string,
) {
  const prefix = outboxPrefixes[kind] + suffix;
  return Array.from({ length: storage.length }, (_, index) =>
    storage.key(index),
  )
    .filter((name): name is string => !!name?.startsWith(prefix))
    .map((name) => name.slice(outboxPrefixes[kind].length));
}

/** Remove retired local copies without touching credentials or request fences. */
export async function cleanupLegacyPersistence(
  storage: Pick<
    Storage,
    "length" | "key" | "removeItem"
  > = globalThis.localStorage,
  factory:
    | Pick<IDBFactory, "databases" | "deleteDatabase">
    | undefined = browserIndexedDB(),
) {
  const retiredKeys = [
    "codex-chat-draft:",
    "codex-drafts:",
    "studio-session-cost:",
    "studio-upload-cancelled:",
    "studio-upload-settled:",
    "studio-workspace-panel-width:",
    "studio-sidebar-width:",
    "studio-removed-messages:",
  ];
  const keys = Array.from({ length: storage.length }, (_, index) =>
    storage.key(index),
  );
  for (const key of keys) {
    if (!key) continue;
    const logical = key.replace(/^:server:[^:]+:/, "");
    if (retiredKeys.some((prefix) => logical.startsWith(prefix)))
      storage.removeItem(key);
  }
  if (!factory) return;
  const retiredDatabases = ["codex-studio-dictation", "codex-studio-uploads"];
  for (const { name } of await factory.databases()) {
    if (
      !name ||
      !retiredDatabases.some(
        (prefix) => name === prefix || name.startsWith(`${prefix}:server:`),
      )
    )
      continue;
    await new Promise<void>((resolve, reject) => {
      const request = factory.deleteDatabase(name);
      request.onsuccess = () => resolve();
      request.onerror = () => reject(request.error);
      request.onblocked = () =>
        reject(new Error("Close older Studio pages to remove retired data."));
    });
  }
}
