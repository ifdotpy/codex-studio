import { LocalCollection, type Change, type Row } from "./localCollection";

/**
 * IndexedDB holds two things here, and nothing else for sync:
 *  - `cache`: one record for each subscription (rows and cursor in one value),
 *    for a frontend that is remote from its server.
 *  - `unsent`: input the server has not confirmed (outbox, unsent drafts).
 * Memory is what the UI reads. The cache is written after render, at most once
 * every `CACHE_WRITE_INTERVAL_MS` and when the page is hidden. Unsent input is
 * written at once and removed when the server confirms it.
 */

/** Longest time between two cache writes, in milliseconds. */
export const CACHE_WRITE_INTERVAL_MS = 5_000;

export type StoreName = "cache" | "unsent";

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
        request.result.createObjectStore("cache");
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

/** The record that holds a row: one per transcript, one for everything else. */
export function recordKey(id: string) {
  const match = /^transcript:(?:item:|cursor:)?([^:]+)/.exec(id);
  return match ? `transcript:${match[1]}` : "entities";
}

export type ProjectionCache = {
  collection: LocalCollection;
  /** Write the changed records now. */
  flush(): Promise<void>;
  stop(): void;
};

/**
 * A projection store for one workspace and API schema. With `kv` it loads each
 * record when first used, and writes changed records after render. Loaded rows
 * never replace newer rows that the stream already wrote.
 */
export function createProjectionCache(
  name: string,
  scope: string,
  kv?: KeyValue,
  timers: Pick<typeof globalThis, "setTimeout" | "clearTimeout"> = globalThis,
): ProjectionCache {
  const loaded = new Map<string, Promise<void>>();
  const dirty = new Set<string>();
  let timer: ReturnType<typeof setTimeout> | undefined;
  let collection!: LocalCollection;
  const keyOf = (key: string) => `${scope}|${key}`;
  const load = (key: string) => {
    let wait = loaded.get(key);
    if (!wait) {
      wait = (async () => {
        try {
          const entries = await kv!.entries("cache", keyOf(key));
          const rows = entries.flatMap(([entry, value]) =>
            entry === keyOf(key)
              ? ((value as { rows?: Row[] } | undefined)?.rows ?? [])
              : [],
          );
          await collection.hydrate(rows);
        } catch {
          // A cache that cannot be read only costs a larger delta.
        }
      })();
      loaded.set(key, wait);
    }
    return wait;
  };
  const flush = async () => {
    clearTimeout(timer);
    timer = undefined;
    if (!kv || !dirty.size) return;
    const keys = [...dirty];
    dirty.clear();
    const rows = collection.all();
    try {
      for (const key of keys)
        await kv.put("cache", keyOf(key), {
          rows: rows.filter((row) => recordKey(row.id) === key),
        });
    } catch {
      // A lost write only makes the next reconnect ask for a larger delta.
    }
  };
  const onChange = (change: Change) => {
    if (!kv || change.context === "cache") return;
    dirty.add(recordKey(change.id));
    timer ??= timers.setTimeout(() => void flush(), CACHE_WRITE_INTERVAL_MS);
  };
  collection = new LocalCollection(name, {
    ...(kv ? { ensure: (id) => load(recordKey(id)) } : {}),
    onChange,
  });
  const hide = () => {
    if (document.visibilityState === "hidden") void flush();
  };
  const leave = () => void flush();
  if (kv && typeof document !== "undefined") {
    document.addEventListener("visibilitychange", hide);
    addEventListener("pagehide", leave);
  }
  return {
    collection,
    flush,
    stop() {
      clearTimeout(timer);
      if (kv && typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", hide);
        removeEventListener("pagehide", leave);
      }
    },
  };
}

/** Delete the cache records of other API schemas for this workspace. */
export async function pruneOtherSchemas(
  kv: KeyValue,
  workspaceScope: string,
  schemaHash: string,
) {
  const entries = await kv.entries("cache", `${workspaceScope}|`);
  await kv.remove(
    "cache",
    entries
      .map(([key]) => key)
      .filter((key) => !key.startsWith(`${workspaceScope}|${schemaHash}|`)),
  );
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
