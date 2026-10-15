// Runs before the application mounts, in a fresh destination browser origin.
// A receipt owns incomplete imports, so a restart can finish the same archive.
export async function importBrowserProfile(snapshot, archiveId, readChunk) {
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
      ![1, 2].includes(snapshot?.version) ||
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

    function decode(value) {
      if (!Array.isArray(value)) throw new Error("Invalid saved value.");
      const [type, item, extra, fourth, fifth] = value;
      switch (type) {
        case "undefined":
          return undefined;
        case "value":
          if (
            item === null ||
            ["string", "boolean"].includes(typeof item) ||
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
          const date = new Date(item);
          if (!Number.isFinite(date.getTime())) break;
          return date;
        }
        case "bytes":
          return Uint8Array.from(atob(item), (c) => c.charCodeAt(0)).buffer;
        case "blob":
          return new Blob([decode(extra)], { type: item });
        case "file":
          return new File([decode(fifth)], item, {
            type: extra,
            lastModified: fourth,
          });
        case "view": {
          const constructors = {
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
          if (!Object.hasOwn(constructors, item)) break;
          return new constructors[item](decode(extra));
        }
        case "array":
          return item.map(decode);
        case "object":
          return Object.fromEntries(
            item.map(([key, child]) => [key, decode(child)]),
          );
      }
      throw new Error("Unsupported saved value.");
    }
    if (
      !snapshot.preferences ||
      typeof snapshot.preferences !== "object" ||
      Array.isArray(snapshot.preferences) ||
      !Array.isArray(snapshot.databases)
    )
      throw new Error("Invalid browser profile contents.");
    for (const [key, value] of Object.entries(snapshot.preferences)) {
      if (key === marker || typeof value !== "string")
        throw new Error("Invalid saved preference.");
    }
    const names = new Set();
    // Decode everything before taking ownership or changing destination storage.
    const databases = snapshot.databases.map((db) => {
      if (
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
              (!Number.isSafeInteger(store.nextKey) || store.nextKey < 1)) ||
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
            records: (store.records || []).map(([key, value]) => [
              decode(key),
              decode(value),
            ]),
          };
        }),
      };
    });
    async function chunkRecords(chunk) {
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
      const records =
        typeof payload === "string" ? JSON.parse(payload) : payload;
      if (!Array.isArray(records) || records.length !== chunk.records)
        throw new Error("The saved profile chunk changed.");
      return records.map(([key, value]) => [decode(key), decode(value)]);
    }
    // Validate all chunks before taking ownership. Keep only one chunk in memory.
    if (snapshot.version === 2)
      for (const db of databases)
        for (const store of db.stores)
          for (const chunk of store.chunks) await chunkRecords(chunk);
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
          (!Object.hasOwn(snapshot.preferences, key) ||
            snapshot.preferences[key] !== localStorage.getItem(key)),
      )
    )
      throw new Error("Preferences changed during an incomplete import.");
    localStorage.setItem(marker, identity + ":pending");
    const request = (operation) =>
      new Promise((resolve, reject) => {
        let blocked;
        operation.onsuccess = () => {
          if (blocked) {
            operation.result?.close?.();
            reject(blocked);
          } else resolve(operation.result);
        };
        operation.onerror = () => reject(operation.error);
        // IndexedDB operations cannot be cancelled. Retain the Web Lock until
        // this operation settles so a late delete cannot erase a newer retry.
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
      let upgradeError;
      opening.onupgradeneeded = () => {
        opening.transaction.onerror = () => {
          upgradeError ??= opening.transaction.error;
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
            if (schema.autoIncrement && schema.nextKey > 1) {
              const key = schema.nextKey - 1;
              const value = {};
              if (schema.keyPath !== null) {
                const parts = schema.keyPath.split(".");
                let parent = value;
                for (const part of parts.slice(0, -1)) {
                  Object.defineProperty(parent, part, {
                    value: {},
                    enumerable: true,
                  });
                  parent = parent[part];
                }
                Object.defineProperty(parent, parts.at(-1), {
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
          opening.transaction.abort();
        }
      };
      try {
        const db = await request(opening);
        try {
          if (snapshot.version === 2) {
            for (const schema of saved.stores)
              for (const chunk of schema.chunks) {
                const records = await chunkRecords(chunk);
                await new Promise((resolve, reject) => {
                  const tx = db.transaction(schema.name, "readwrite");
                  let failure;
                  tx.oncomplete = resolve;
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
    for (const [key, value] of Object.entries(snapshot.preferences))
      localStorage.setItem(key, value);
    localStorage.setItem(marker, identity + ":complete");
  }
}
