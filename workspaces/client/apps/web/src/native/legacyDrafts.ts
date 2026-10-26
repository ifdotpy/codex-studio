import { serverLocalStorage } from "../servers/storage";
import { serverDatabaseSuffix } from "../servers/environment";
import { readLegacyDraftValues } from "./legacyDraftValues";
export { readLegacyDraftValues } from "./legacyDraftValues";
import type { DraftVersion } from "../sync/drafts";

// Read the previous browser stores without starting replication or writing to
// them. Keep the original records until the native receipt has been committed.
export async function readLegacyDrafts(
  workspace: string,
  options: {
    storage?: Storage;
    indexedDB?: IDBFactory;
    databaseSuffix?: string;
  } = {},
): Promise<DraftVersion[]> {
  const storage = options.storage ?? serverLocalStorage;
  const factory = options.indexedDB ?? indexedDB;
  const versions: DraftVersion[] = [];
  const name = `rxdb-dexie-studio${workspace}${options.databaseSuffix ?? serverDatabaseSuffix}--0--drafts`;
  const databases = await factory.databases();
  const saved = databases.find((db) => db.name === name);
  if (saved) {
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const request = factory.open(name, saved.version);
      request.onupgradeneeded = () => request.transaction?.abort();
      request.onerror = () => reject(request.error);
      request.onsuccess = () => resolve(request.result);
    });
    try {
      const rows = await new Promise<
        Array<{ id: string; payload: string; _deleted: boolean | number }>
      >((resolve, reject) => {
        const transaction = db.transaction("docs", "readonly");
        const request = transaction.objectStore("docs").getAll();
        transaction.oncomplete = () => resolve(request.result);
        transaction.onabort = () => reject(transaction.error);
      });
      for (const row of rows) {
        if (row._deleted) continue;
        const version = JSON.parse(row.payload) as DraftVersion;
        if (version.id !== row.id)
          throw new Error("Saved draft identity does not match its record.");
        versions.push(version);
      }
    } finally {
      db.close();
    }
  }
  return readLegacyDraftValues(workspace, storage, versions);
}

export async function importLegacyDrafts(
  workspace: string,
  invoke: <T>(method: string, params: unknown, id?: string) => Promise<T>,
) {
  const versions = await readLegacyDrafts(workspace);
  if (!versions.length) return;
  const params = { workspace, versions };
  const hash = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(JSON.stringify(params)),
  );
  const id =
    "browser-drafts:" +
    Array.from(new Uint8Array(hash), (byte) =>
      byte.toString(16).padStart(2, "0"),
    ).join("");
  await invoke("drafts.import", params, id);
}
