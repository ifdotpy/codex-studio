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
  chatCreation: "codex-pending-creation:",
  projectSave: "studio-peer-convert:",
  queueChange: "studio-queue-change:",
  removeSending: "studio-remove-sending:",
  removedSend: "studio-removed-send:",
  sidebarOrder: "codex-sidebar-order:",
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

/** Serialize queue identity changes across tabs using the existing lock database. */
export function createQueueOutbox(storage: Storage, databaseName: string) {
  return async function updateQueueRequest<T>(
    suffix: string,
    fallback: T,
    update: (current: T) => T,
  ): Promise<T> {
    const key = outboxRequests(storage).key("queueChange", suffix);
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const request = browserIndexedDB()!.open(databaseName, 1);
      request.onupgradeneeded = () =>
        request.result.createObjectStore("writes");
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    });
    return new Promise((resolve, reject) => {
      const tx = db.transaction("writes", "readwrite");
      let result: T;
      let failure: unknown;
      tx.objectStore("writes").get(key).onsuccess = () => {
        try {
          const raw = storage.getItem(key);
          result = update(raw === null ? fallback : JSON.parse(raw));
          if (result === null) storage.removeItem(key);
          else storage.setItem(key, JSON.stringify(result));
        } catch (error) {
          failure = error;
          tx.abort();
        }
      };
      tx.oncomplete = () => {
        db.close();
        resolve(result!);
      };
      tx.onerror = tx.onabort = () => {
        db.close();
        reject(
          failure ||
            tx.error ||
            new Error("Queue changes could not be saved on this device."),
        );
      };
    });
  };
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
    "codex-queue-edit:",
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
    if (
      retiredKeys.some((prefix) => logical.startsWith(prefix)) ||
      (logical.startsWith(outboxPrefixes.sidebarOrder) &&
        !logical.endsWith(":pending")) ||
      (logical.startsWith(outboxPrefixes.chatCreation) &&
        /:(?:rejected|dismissed):[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(
          logical,
        ) &&
        !logical.includes(":request:"))
    )
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

export const PAIRED_SERVERS_KEY = "studio-paired-servers-v1";

export type PairedServerReference = {
  id: string;
  origin: string;
  credentialId: string;
};

export function readPairedServerReferences(
  storage: Storage = globalThis.localStorage,
): PairedServerReference[] {
  const rows: unknown = JSON.parse(
    storage?.getItem(PAIRED_SERVERS_KEY) || "[]",
  );
  if (!Array.isArray(rows))
    throw new Error("The saved server list is invalid.");
  return rows.map((row) => {
    if (
      !row ||
      typeof row.id !== "string" ||
      typeof row.origin !== "string" ||
      typeof row.credentialId !== "string"
    )
      throw new Error("The saved server list is invalid.");
    return { id: row.id, origin: row.origin, credentialId: row.credentialId };
  });
}

export function writePairedServerReferences(
  servers: readonly PairedServerReference[],
  storage: Storage = globalThis.localStorage,
): void {
  storage.setItem(
    PAIRED_SERVERS_KEY,
    JSON.stringify(
      servers
        .map(({ id, origin, credentialId }) => ({ id, origin, credentialId }))
        .sort((a, b) => a.id.localeCompare(b.id)),
    ),
  );
}
