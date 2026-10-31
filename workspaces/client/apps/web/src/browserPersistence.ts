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
