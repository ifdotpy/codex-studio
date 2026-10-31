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

/** Remove retired local copies without touching credentials or request fences. */
export async function cleanupLegacyPersistence(
  storage: Storage = globalThis.localStorage,
  factory: IDBFactory | undefined = browserIndexedDB(),
) {
  const retiredKeys = [
    "codex-chat-draft:",
    "codex-drafts:",
    "studio-session-cost:",
    "studio-upload-cancelled:",
    "studio-upload-settled:",
    "studio-workspace-panel-width:",
    "studio-sidebar-width:",
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
