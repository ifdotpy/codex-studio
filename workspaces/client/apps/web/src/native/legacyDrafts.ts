import { serverLocalStorage } from "../servers/storage";
import { serverDatabaseSuffix } from "../servers/environment";
import { readDraftJournal } from "../sync/draftJournal";
import { readLocalDraftRecord } from "../sync/draftStorage";
import type { DraftVersion } from "../sync/drafts";

// Read the previous browser stores without starting replication or writing to
// them. Keep the original records until the native receipt has been committed.
export async function readLegacyDrafts(
  workspace: string,
): Promise<DraftVersion[]> {
  const scope = `codex-drafts:${workspace}`;
  const versions: DraftVersion[] = [];
  const name = `rxdb-dexie-studio${workspace}${serverDatabaseSuffix}--0--drafts`;
  const databases = await indexedDB.databases();
  const saved = databases.find((db) => db.name === name);
  if (saved) {
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const request = indexedDB.open(name, saved.version);
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
  for (const entry of readDraftJournal(scope)) versions.push(entry.version);
  const prefix = `codex-chat-draft:${workspace}:`;
  const sessions = Array.from(
    { length: serverLocalStorage.length },
    (_, index) => serverLocalStorage.key(index)!,
  )
    .filter((key) => key.startsWith(prefix))
    .map((key) => decodeURIComponent(key.slice(prefix.length)))
    .sort();
  for (const session of sessions) {
    const record = readLocalDraftRecord(scope, session)!;
    const text = record.deleted ? "" : record.text;
    // A local recovery copy is an independent branch. Do not overwrite another
    // device's branch or discard divergent text during the migration.
    if (
      !versions.some(
        (version) => version.session === session && version.text === text,
      )
    ) {
      versions.push({
        id: `browser-import:${session}`,
        device: "browser-import",
        session,
        text,
        updated:
          record.updated ??
          (record.source === "local"
            ? Math.max(
                0,
                ...versions
                  .filter((version) => version.session === session)
                  .map((version) => version.updated + 1),
              )
            : 0),
      });
    }
  }
  // Legacy scope-wide snapshots predate per-session records.
  const raw = serverLocalStorage.getItem(scope);
  if (raw !== null) {
    const legacy: unknown = JSON.parse(raw);
    if (!legacy || typeof legacy !== "object" || Array.isArray(legacy))
      throw new Error("Invalid saved draft snapshot.");
    for (const [session, text] of Object.entries(legacy).sort()) {
      if (typeof text !== "string")
        throw new Error("Invalid saved draft text.");
      if (
        text &&
        !readLocalDraftRecord(scope, session) &&
        !versions.some(
          (version) => version.session === session && version.text === text,
        )
      )
        versions.push({
          id: `browser-snapshot:${session}`,
          device: "browser-snapshot",
          session,
          text,
          updated: 0,
        });
    }
  }
  return versions;
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
