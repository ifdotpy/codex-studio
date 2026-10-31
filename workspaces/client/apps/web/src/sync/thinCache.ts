import type { Row } from "./localCollection";

/** Unconfirmed outbox and draft rows keep their request identities locally. */
export type StoreName = "unsent";

export type KeyValue = {
  entries(store: StoreName, prefix: string): Promise<Array<[string, unknown]>>;
  put(store: StoreName, key: string, value: unknown): Promise<void>;
  remove(store: StoreName, keys: string[]): Promise<void>;
};

export const THIN_CACHE_DATABASE = "studio-thin-cache";

function done<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

function finished(transaction: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => reject(transaction.error);
    transaction.onabort = () => reject(transaction.error);
  });
}

/** The plain IndexedDB store; `undefined` where the browser has none. */
export function indexedDbKeyValue(
  factory: IDBFactory | undefined = globalThis.indexedDB,
): KeyValue | undefined {
  if (!factory) return undefined;
  let opened: Promise<IDBDatabase> | undefined;
  const database = () =>
    (opened ??= new Promise<IDBDatabase>((resolve, reject) => {
      const request = factory.open(THIN_CACHE_DATABASE, 1);
      request.onupgradeneeded = () => {
        request.result.createObjectStore("unsent");
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error);
    }).catch((error) => {
      opened = undefined;
      throw error;
    }));
  const range = (prefix: string) => IDBKeyRange.bound(prefix, `${prefix}￿`);
  return {
    async entries(store, prefix) {
      const db = await database();
      const transaction = db.transaction(store, "readonly");
      const keys = await done(
        transaction.objectStore(store).getAllKeys(range(prefix)),
      );
      const values = await done(
        transaction.objectStore(store).getAll(range(prefix)),
      );
      return keys.map((key, index) => [String(key), values[index]]);
    },
    async put(store, key, value) {
      const db = await database();
      const transaction = db.transaction(store, "readwrite");
      transaction.objectStore(store).put(value, key);
      await finished(transaction);
    },
    async remove(store, keys) {
      if (!keys.length) return;
      const db = await database();
      const transaction = db.transaction(store, "readwrite");
      for (const key of keys) transaction.objectStore(store).delete(key);
      await finished(transaction);
    },
  };
}

export type UnsentValue = { row: Row; assumed?: Row | null };

/** Unsent input of one workspace, by collection. Written at once. */
export function createUnsentStore(kv: KeyValue | undefined, scope: string) {
  const keyOf = (collection: string, id: string) =>
    `${scope}|${collection}|${id}`;
  return {
    async load(collection: string): Promise<UnsentValue[]> {
      if (!kv) return [];
      const entries = await kv.entries("unsent", `${scope}|${collection}|`);
      return entries.map(([, value]) => value as UnsentValue);
    },
    async put(collection: string, value: UnsentValue) {
      await kv?.put("unsent", keyOf(collection, value.row.id), value);
    },
    async remove(collection: string, id: string) {
      await kv?.remove("unsent", [keyOf(collection, id)]);
    },
  };
}
export type UnsentStore = ReturnType<typeof createUnsentStore>;

/** An outbox row whose message the server confirmed, or that was cancelled, holds no unsent input. */
export function outboxSettled(payload: string) {
  try {
    const value: unknown = JSON.parse(payload);
    const status =
      value && typeof value === "object" && "status" in value
        ? value.status
        : undefined;
    return status === "accepted" || status === "cancelled";
  } catch {
    return false;
  }
}
