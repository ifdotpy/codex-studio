import { outboxSettled, type KeyValue } from "./thinCache";

/**
 * The previous sync client kept drafts and the outbox in RxDB databases on
 * Dexie. Move their unsent input to the thin store once, then delete every
 * database of that client. Nothing is deleted unless every row was moved.
 */

const LEGACY_PREFIX = "rxdb-dexie-";
const LEGACY_DOCS =
  /^rxdb-dexie-studio([a-f0-9]{32})([a-z0-9]*)--0--(drafts|outbox)$/;
const MIGRATION_LOCK = "studio-thin-cache-migration";

type LegacyRow = {
  id: string;
  payload: string;
  seq?: number;
  _deleted?: boolean | number;
};

function readDocs(
  factory: IDBFactory,
  name: string,
  version: number | undefined,
) {
  return new Promise<LegacyRow[]>((resolve, reject) => {
    const open = factory.open(name, version);
    // Reading must never create or upgrade a database.
    open.onupgradeneeded = () => open.transaction?.abort();
    open.onerror = () => reject(open.error);
    open.onsuccess = () => {
      const db = open.result;
      if (!db.objectStoreNames.contains("docs")) {
        db.close();
        resolve([]);
        return;
      }
      const transaction = db.transaction("docs", "readonly");
      const request = transaction.objectStore("docs").getAll();
      transaction.oncomplete = () => {
        db.close();
        resolve(request.result);
      };
      transaction.onabort = () => {
        db.close();
        reject(transaction.error);
      };
    };
  });
}

function drop(factory: IDBFactory, name: string) {
  return new Promise<void>((resolve) => {
    const request = factory.deleteDatabase(name);
    request.onsuccess = () => resolve();
    request.onerror = () => resolve();
    // An old tab still holds it open; the browser deletes it when that tab closes.
    request.onblocked = () => resolve();
  });
}

async function migrate(kv: KeyValue, factory: IDBFactory) {
  const databases = await factory.databases?.();
  if (!databases) return;
  const legacy = databases.filter(({ name }) =>
    name?.startsWith(LEGACY_PREFIX),
  );
  for (const { name, version } of legacy) {
    const match = LEGACY_DOCS.exec(name!);
    if (!match) continue;
    const [, workspace, suffix, collection] = match;
    for (const row of await readDocs(factory, name!, version)) {
      if (row._deleted || typeof row.payload !== "string") continue;
      if (collection === "outbox" && outboxSettled(row.payload)) continue;
      await kv.put("unsent", `${workspace}${suffix}|${collection}|${row.id}`, {
        row: {
          id: row.id,
          payload: row.payload,
          seq: Number.isSafeInteger(row.seq) ? row.seq : 0,
        },
        assumed: null,
      });
    }
  }
  for (const { name } of legacy) await drop(factory, name!);
}

/** Run the move once per browser profile; later starts find nothing to do. */
export async function migrateLegacyStores(
  kv: KeyValue | undefined,
  factory: IDBFactory | undefined = globalThis.indexedDB,
) {
  if (!kv || !factory) return;
  const run = () => migrate(kv, factory);
  if (typeof navigator !== "undefined" && navigator.locks?.request)
    await navigator.locks.request(MIGRATION_LOCK, run);
  else await run();
}
