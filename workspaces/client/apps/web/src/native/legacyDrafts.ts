import { serverDatabaseSuffix } from "../servers/environment";
import { readDraftJournal } from "../sync/draftJournal";
import { readLocalDraftRecord } from "../sync/draftStorage";
import type { DraftVersion } from "../generated/rustOwned";

type Invoke = <T>(method: string, params: unknown, id?: string) => Promise<T>;
type Edit = { id: string; changes: Record<string, string> };

/** Key prefix of the pending native edits that earlier builds kept in the browser store. */
export const DRAFT_JOURNAL_PREFIX = "studio-native-draft-journal-v1:";

// preferences.ts passes the browser store before it replaces window.localStorage.
let browserStorage: Storage | undefined;
export function setLegacyDraftStorage(storage: Storage) {
  browserStorage = storage;
}
function browser(): Storage {
  if (!browserStorage)
    throw new Error("The browser draft store is unavailable.");
  return browserStorage;
}

// Read the previous browser stores without starting replication or writing to
// them. Keep the original records until the native receipt has been committed.
export async function readLegacyDrafts(
  workspace: string,
): Promise<DraftVersion[]> {
  const storage = browser();
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
  for (const entry of readDraftJournal(storage, scope))
    versions.push(entry.version);
  const prefix = `codex-chat-draft:${workspace}:`;
  const sessions = Array.from({ length: storage.length }, (_, index) =>
    storage.key(index)!,
  )
    .filter((key) => key.startsWith(prefix))
    .map((key) => decodeURIComponent(key.slice(prefix.length)))
    .sort();
  for (const session of sessions) {
    const record = readLocalDraftRecord(storage, scope, session)!;
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
  const raw = storage.getItem(scope);
  if (raw !== null) {
    const legacy: unknown = JSON.parse(raw);
    if (!legacy || typeof legacy !== "object" || Array.isArray(legacy))
      throw new Error("Invalid saved draft snapshot.");
    for (const [session, text] of Object.entries(legacy).sort()) {
      if (typeof text !== "string")
        throw new Error("Invalid saved draft text.");
      if (
        text &&
        !readLocalDraftRecord(storage, scope, session) &&
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

// Pending native edits, in the order they were written. Corrupt rows stop the import.
function readNativeJournal(workspace: string) {
  const storage = browser();
  const prefix = DRAFT_JOURNAL_PREFIX + encodeURIComponent(workspace) + ":";
  const rows: { sequence: number; key: string; edit: Edit }[] = [];
  for (let index = 0; index < storage.length; index++) {
    const key = storage.key(index)!;
    if (!key.startsWith(prefix)) continue;
    const row = JSON.parse(storage.getItem(key)!);
    if (
      row?.version !== 1 ||
      row.workspace !== workspace ||
      !Number.isSafeInteger(row.sequence) ||
      row.sequence < 1 ||
      typeof row.edit?.id !== "string" ||
      !row.edit.id ||
      row.edit.id.length > 128 ||
      key !== prefix + row.edit.id ||
      !row.edit.changes ||
      typeof row.edit.changes !== "object" ||
      Array.isArray(row.edit.changes) ||
      !Object.entries(row.edit.changes).every(
        ([name, value]) => name && typeof value === "string",
      )
    )
      throw new Error(
        "Saved draft changes could not be read. Keep the recovery journal.",
      );
    rows.push({ sequence: row.sequence, key, edit: row.edit });
  }
  return rows.sort(
    (a, b) => a.sequence - b.sequence || a.edit.id.localeCompare(b.edit.id),
  );
}

export async function importLegacyDrafts(workspace: string, invoke: Invoke) {
  const versions = await readLegacyDrafts(workspace);
  const pending = readNativeJournal(workspace);
  if (versions.length) {
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
  // The browser copy goes only after the core queue has accepted the edit.
  for (const { key, edit } of pending) {
    await invoke("drafts.queue", { workspace, changes: edit.changes }, edit.id);
    browser().removeItem(key);
  }
}
