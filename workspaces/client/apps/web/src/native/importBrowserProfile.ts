export type EncodedProfileValue =
  | ["undefined"]
  | ["value", null | string | boolean | number]
  | ["number", string]
  | ["date", string]
  | ["bytes", string]
  | ["blob", string, EncodedProfileValue]
  | ["file", string, string, number, EncodedProfileValue]
  | ["view", string, EncodedProfileValue]
  | ["array", EncodedProfileValue[]]
  | ["object", [string, EncodedProfileValue][]];
export type BrowserProfileRecord = [EncodedProfileValue, EncodedProfileValue];
export type BrowserProfileChunk = {
  sha256: string;
  bytes: number;
  records: number;
  owner?: string;
};
export type BrowserProfileStore = {
  name: string;
  keyPath: string | string[] | null;
  autoIncrement: boolean;
  nextKey?: number;
  indexes: {
    name: string;
    keyPath: string | string[];
    unique: boolean;
    multiEntry: boolean;
  }[];
  records?: BrowserProfileRecord[];
  chunks?: BrowserProfileChunk[];
};
export type BrowserProfileDatabase = {
  name: string;
  version: number;
  stores: BrowserProfileStore[];
};
export type BrowserProfileSnapshot = {
  version: 1 | 2;
  origin?: string;
  topLevelOrigin?: string;
  preferences: Record<string, string>;
  databases: BrowserProfileDatabase[];
};

// Runs before the application mounts, in a fresh destination browser origin.
// A receipt owns incomplete imports, so a restart can finish the same archive.
export async function importBrowserProfile(
  snapshot: unknown,
  archiveId: string,
  readChunk?: (chunk: BrowserProfileChunk) => unknown,
): Promise<void> {
  const record = (value: unknown): value is Record<string, unknown> =>
    typeof value === "object" && value !== null && !Array.isArray(value);
  const list = (value: unknown): value is unknown[] => Array.isArray(value);
  const keyPath = (value: unknown): value is string | string[] =>
    typeof value === "string" ||
    (list(value) && value.every((part) => typeof part === "string"));
  function savedStore(value: unknown): value is BrowserProfileStore {
    return (
      record(value) &&
      typeof value.name === "string" &&
      (value.keyPath === null || keyPath(value.keyPath)) &&
      typeof value.autoIncrement === "boolean" &&
      (value.nextKey === undefined || typeof value.nextKey === "number") &&
      list(value.indexes) &&
      value.indexes.every(
        (index) =>
          record(index) &&
          typeof index.name === "string" &&
          keyPath(index.keyPath) &&
          typeof index.unique === "boolean" &&
          typeof index.multiEntry === "boolean",
      ) &&
      (value.records === undefined ||
        (list(value.records) &&
          value.records.every((entry) => list(entry) && entry.length === 2))) &&
      (value.chunks === undefined ||
        (list(value.chunks) &&
          value.chunks.every(
            (chunk) =>
              record(chunk) &&
              typeof chunk.sha256 === "string" &&
              typeof chunk.bytes === "number" &&
              typeof chunk.records === "number",
          )))
    );
  }
  function savedDatabase(value: unknown): value is BrowserProfileDatabase {
    return (
      record(value) &&
      typeof value.name === "string" &&
      typeof value.version === "number" &&
      list(value.stores) &&
      value.stores.every(savedStore)
    );
  }
  const validKey = (value: unknown): value is IDBValidKey =>
    typeof value === "number" ||
    typeof value === "string" ||
    value instanceof Date ||
    value instanceof ArrayBuffer ||
    ArrayBuffer.isView(value) ||
    (list(value) && value.every(validKey));
  const decodeKey = (value: unknown): IDBValidKey | undefined => {
    const key = decode(value);
    if (key === undefined || validKey(key)) return key;
    throw new Error("Unsupported saved value.");
  };
  function decode(value: unknown): unknown {
    if (!list(value)) throw new Error("Invalid saved value.");
    const [type, item, extra, fourth, fifth] = value;
    switch (type) {
      case "undefined":
        return undefined;
      case "value":
        if (
          item === null ||
          typeof item === "string" ||
          typeof item === "boolean" ||
          (typeof item === "number" && Number.isFinite(item))
        )
          return item;
        break;
      case "number":
        if (item === "-0") return -0;
        if (item === "NaN") return NaN;
        if (item === "Infinity") return Infinity;
        if (item === "-Infinity") return -Infinity;
        break;
      case "date": {
        if (typeof item !== "string") break;
        const date = new Date(item);
        if (!Number.isFinite(date.getTime())) break;
        return date;
      }
      case "bytes":
        if (typeof item === "string")
          return Uint8Array.from(atob(item), (c) => c.charCodeAt(0)).buffer;
        break;
      case "blob": {
        const bytes = decode(extra);
        if (typeof item === "string" && bytes instanceof ArrayBuffer)
          return new Blob([bytes], { type: item });
        break;
      }
      case "file": {
        const bytes = decode(fifth);
        if (
          typeof item === "string" &&
          typeof extra === "string" &&
          typeof fourth === "number" &&
          bytes instanceof ArrayBuffer
        )
          return new File([bytes], item, { type: extra, lastModified: fourth });
        break;
      }
      case "view": {
        const constructors: Record<
          string,
          new (buffer: ArrayBuffer) => ArrayBufferView
        > = {
          Int8Array,
          Uint8Array,
          Uint8ClampedArray,
          Int16Array,
          Uint16Array,
          Int32Array,
          Uint32Array,
          Float32Array,
          Float64Array,
          BigInt64Array,
          BigUint64Array,
          DataView,
        };
        if (typeof item !== "string" || !Object.hasOwn(constructors, item))
          break;
        const bytes = decode(extra);
        if (!(bytes instanceof ArrayBuffer)) break;
        const Constructor = constructors[item];
        if (!Constructor) break;
        return new Constructor(bytes);
      }
      case "array":
        if (list(item)) return item.map(decode);
        break;
      case "object":
        if (list(item))
          return Object.fromEntries(
            item.map((entry) => {
              if (!list(entry) || typeof entry[0] !== "string")
                throw new Error("Invalid saved value.");
              return [entry[0], decode(entry[1])];
            }),
          );
        break;
    }
    throw new Error("Unsupported saved value.");
  }
  if (!navigator.locks)
    throw new Error("This browser cannot lock profile imports.");
  return navigator.locks.request(
    "studio-browser-profile-import",
    { ifAvailable: true },
    async (lock) => {
      if (!lock) throw new Error("Another view is importing this profile.");
      return restore();
    },
  );
  async function restore() {
    const marker = "studio-browser-profile-import-v1";
    if (
      !record(snapshot) ||
      (snapshot.version !== 1 && snapshot.version !== 2) ||
      typeof archiveId !== "string" ||
      !archiveId
    )
      throw new Error("Invalid browser profile archive.");
    const digest = await crypto.subtle.digest(
      "SHA-256",
      new TextEncoder().encode(JSON.stringify(snapshot)),
    );
    const identity =
      archiveId +
      ":" +
      Array.from(new Uint8Array(digest), (v) =>
        v.toString(16).padStart(2, "0"),
      ).join("");
    const previous = localStorage.getItem(marker);
    if (previous === identity + ":complete") return;
    if (previous && previous !== identity + ":pending")
      throw new Error("This browser profile belongs to a different import.");

    if (!record(snapshot.preferences) || !list(snapshot.databases))
      throw new Error("Invalid browser profile contents.");
    const preferences: Record<string, string> = {};
    for (const [key, value] of Object.entries(snapshot.preferences)) {
      if (key === marker || typeof value !== "string")
        throw new Error("Invalid saved preference.");
      preferences[key] = value;
    }
    const names = new Set();
    // Decode everything before taking ownership or changing destination storage.
    const databases = snapshot.databases.map((db) => {
      if (
        !savedDatabase(db) ||
        typeof db.name !== "string" ||
        !Number.isSafeInteger(db.version) ||
        db.version < 1 ||
        names.has(db.name) ||
        !Array.isArray(db.stores)
      )
        throw new Error("Invalid saved database.");
      names.add(db.name);
      const stores = new Set();
      return {
        ...db,
        stores: db.stores.map((store) => {
          if (
            typeof store.name !== "string" ||
            stores.has(store.name) ||
            typeof store.autoIncrement !== "boolean" ||
            (store.autoIncrement &&
              (typeof store.nextKey !== "number" ||
                !Number.isSafeInteger(store.nextKey) ||
                store.nextKey < 1)) ||
            !Array.isArray(store.indexes) ||
            (snapshot.version === 1
              ? !Array.isArray(store.records)
              : store.records !== undefined ||
                !Array.isArray(store.chunks) ||
                store.chunks.some(
                  (chunk) =>
                    !chunk ||
                    !/^[a-f0-9]{64}$/.test(chunk.sha256) ||
                    !Number.isSafeInteger(chunk.bytes) ||
                    chunk.bytes < 2 ||
                    chunk.bytes > 64 * 1024 * 1024 ||
                    !Number.isSafeInteger(chunk.records) ||
                    chunk.records < 1,
                ))
          )
            throw new Error("Invalid saved database store.");
          stores.add(store.name);
          return {
            ...store,
            records: (store.records || []).map(
              ([key, value]): [IDBValidKey | undefined, unknown] => [
                decodeKey(key),
                decode(value),
              ],
            ),
          };
        }),
      };
    });
    async function chunkRecords(
      chunk: BrowserProfileChunk,
    ): Promise<[IDBValidKey | undefined, unknown][]> {
      if (typeof readChunk !== "function")
        throw new Error("Profile chunk reader is unavailable.");
      const payload = await readChunk(chunk);
      const text =
        typeof payload === "string" ? payload : JSON.stringify(payload);
      const bytes = new TextEncoder().encode(text);
      const digest = await crypto.subtle.digest("SHA-256", bytes);
      const hash = Array.from(new Uint8Array(digest), (value) =>
        value.toString(16).padStart(2, "0"),
      ).join("");
      if (bytes.length !== chunk.bytes || hash !== chunk.sha256)
        throw new Error("The saved profile chunk changed.");
      const records: unknown =
        typeof payload === "string" ? JSON.parse(payload) : payload;
      if (!list(records) || records.length !== chunk.records)
        throw new Error("The saved profile chunk changed.");
      return records.map((entry) => {
        if (!list(entry)) throw new Error("Invalid saved value.");
        return [decodeKey(entry[0]), decode(entry[1])];
      });
    }
    // Validate all chunks before taking ownership. Keep only one chunk in memory.
    if (snapshot.version === 2)
      for (const db of databases)
        for (const store of db.stores)
          for (const chunk of store.chunks ?? []) await chunkRecords(chunk);
    const existing = await indexedDB.databases();
    if (!previous && (existing.length || localStorage.length))
      throw new Error("Import requires an empty destination browser profile.");
    if (previous && existing.some((db) => !names.has(db.name)))
      throw new Error("Unexpected database in an incomplete import.");
    if (
      previous &&
      Array.from({ length: localStorage.length }, (_, i) =>
        localStorage.key(i),
      ).some(
        (key) =>
          key !== marker &&
          (key === null ||
            !Object.hasOwn(preferences, key) ||
            preferences[key] !== localStorage.getItem(key)),
      )
    )
      throw new Error("Preferences changed during an incomplete import.");
    localStorage.setItem(marker, identity + ":pending");
    const request = <T>(operation: IDBRequest<T>) =>
      new Promise<T>((resolve, reject) => {
        let blocked: Error | undefined;
        operation.onsuccess = () => {
          if (blocked) {
            if (operation.result instanceof IDBDatabase)
              operation.result.close();
            reject(blocked);
          } else resolve(operation.result);
        };
        operation.onerror = () => reject(operation.error);
        // IndexedDB operations cannot be cancelled. Retain the Web Lock until
        // this operation settles so a late delete cannot erase a newer retry.
        if ("onblocked" in operation)
          operation.onblocked = () => {
            blocked = new Error(
              "Close other views before importing the profile.",
            );
          };
      });
    for (const saved of databases) {
      // Only an import-owned destination is reset. The source profile is never changed.
      await request(indexedDB.deleteDatabase(saved.name));
      const opening = indexedDB.open(saved.name, saved.version);
      let upgradeError: unknown;
      opening.onupgradeneeded = () => {
        const transaction = opening.transaction;
        if (!transaction)
          throw new Error("Missing profile upgrade transaction.");
        transaction.onerror = () => {
          upgradeError ??= transaction.error;
        };
        try {
          for (const schema of saved.stores) {
            const store = opening.result.createObjectStore(schema.name, {
              keyPath: schema.keyPath,
              autoIncrement: schema.autoIncrement,
            });
            for (const index of schema.indexes)
              store.createIndex(index.name, index.keyPath, {
                unique: index.unique,
                multiEntry: index.multiEntry,
              });
            if (
              schema.autoIncrement &&
              schema.nextKey !== undefined &&
              schema.nextKey > 1
            ) {
              const key = schema.nextKey - 1;
              const value: Record<string, unknown> = {};
              if (typeof schema.keyPath === "string") {
                const parts = schema.keyPath.split(".");
                let parent: Record<string, unknown> = value;
                for (const part of parts.slice(0, -1)) {
                  const child: Record<string, unknown> = {};
                  Object.defineProperty(parent, part, {
                    value: child,
                    enumerable: true,
                  });
                  parent = child;
                }
                Object.defineProperty(parent, parts.at(-1) ?? "", {
                  value: key,
                  enumerable: true,
                });
              }
              if (schema.keyPath === null) store.add(value, key);
              else store.add(value);
              store.delete(key);
            }
            for (const [key, value] of schema.records) {
              const written =
                schema.keyPath === null
                  ? store.add(value, key)
                  : store.add(value);
              written.onerror = () => {
                upgradeError = new Error(
                  `Store ${schema.name}: ${String(written.error)}`,
                );
              };
            }
          }
        } catch (error) {
          upgradeError = error;
          transaction.abort();
        }
      };
      try {
        const db = await request(opening);
        try {
          if (snapshot.version === 2) {
            for (const schema of saved.stores)
              for (const chunk of schema.chunks ?? []) {
                const records = await chunkRecords(chunk);
                await new Promise<void>((resolve, reject) => {
                  const tx = db.transaction(schema.name, "readwrite");
                  let failure: unknown;
                  tx.oncomplete = () => resolve();
                  tx.onerror = tx.onabort = () =>
                    reject(
                      failure ||
                        tx.error ||
                        new Error("Profile chunk transaction aborted"),
                    );
                  try {
                    const store = tx.objectStore(schema.name);
                    for (const [key, value] of records) {
                      const write =
                        schema.keyPath === null
                          ? store.add(value, key)
                          : store.add(value);
                      write.onerror = () => {
                        failure = write.error;
                      };
                    }
                  } catch (error) {
                    failure = error;
                    tx.abort();
                  }
                });
              }
          }
        } finally {
          db.close();
        }
      } catch (error) {
        throw new Error(
          `Could not restore database ${saved.name}: ${String(upgradeError || error)}`,
        );
      }
    }
    for (const [key, value] of Object.entries(preferences))
      localStorage.setItem(key, value);
    localStorage.setItem(marker, identity + ":complete");
  }
}

/** Archive envelope shared by the offline exporter and native import checks. */
export type BrowserProfileArchive = {
  version: 1 | 2;
  parts?: string;
  id: string;
  profile?: string;
  created?: number;
  snapshots: BrowserProfileSnapshot[];
  owners: Record<string, string>;
};
