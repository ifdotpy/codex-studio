// Keep this fixed reader self-contained. Each host selects the legacy origin.
export async function exportBrowserProfile(emitChunk, capture = null) {
  if (
    capture !== null &&
    (capture.readOnly !== true ||
      !Number.isSafeInteger(capture.maxRecordBytes) ||
      capture.maxRecordBytes < 1 ||
      typeof capture.expectedOrigin !== "string" ||
      capture.expectedOrigin !==
        (location.protocol === "tauri:" || location.protocol === "studio-view:"
          ? `${location.protocol}//${location.host}`
          : location.origin))
  )
    throw new Error("The native source capture identity is invalid.");
  let capturedBytes = 0;
  let reservedRecordBytes = 0;
  function checkCapture() {
    if (capture?.isCancelled?.())
      throw new Error(
        "The native source capture was canceled. The source was preserved.",
      );
  }
  function reserveBinary(bytes) {
    if (!capture) return;
    if (!Number.isSafeInteger(bytes) || bytes < 0)
      throw new Error("The native source byte bound is invalid.");
    // Base64 alone must fit before File/Blob reads or typed-view copies start.
    const encoded = 4 * Math.ceil(bytes / 3);
    if (encoded > capture.maxRecordBytes - capturedBytes - reservedRecordBytes)
      throw new Error(
        "The native source capture exceeds its record bound. The source was preserved.",
      );
    reservedRecordBytes += encoded;
  }
  async function encode(value, binaryReserved = false) {
    checkCapture();
    if (value === undefined) return ["undefined"];
    if (value === null || ["string", "boolean"].includes(typeof value))
      return ["value", value];
    if (typeof value === "number") {
      if (Object.is(value, -0)) return ["number", "-0"];
      if (!Number.isFinite(value)) return ["number", String(value)];
      return ["value", value];
    }
    if (value instanceof Date) return ["date", value.toISOString()];
    if (value instanceof Blob) {
      reserveBinary(value.size);
      const bytes = await value.arrayBuffer();
      if (capture && bytes.byteLength !== value.size)
        throw new Error(
          "The native source byte length changed. The source was preserved.",
        );
      const encoded = await encode(bytes, true);
      return value instanceof File
        ? ["file", value.name, value.type, value.lastModified, encoded]
        : ["blob", value.type, encoded];
    }
    if (value instanceof ArrayBuffer) {
      if (!binaryReserved) reserveBinary(value.byteLength);
      const bytes = new Uint8Array(value);
      const chunks = [];
      for (let offset = 0; offset < bytes.length; offset += 16384)
        chunks.push(
          String.fromCharCode(...bytes.subarray(offset, offset + 16384)),
        );
      return ["bytes", btoa(chunks.join(""))];
    }
    if (ArrayBuffer.isView(value)) {
      reserveBinary(value.byteLength);
      return [
        "view",
        value.constructor.name,
        await encode(
          value.buffer.slice(
            value.byteOffset,
            value.byteOffset + value.byteLength,
          ),
          true,
        ),
      ];
    }
    if (Array.isArray(value))
      return ["array", await Promise.all(value.map((item) => encode(item)))];
    if (Object.getPrototypeOf(value) === Object.prototype)
      return [
        "object",
        await Promise.all(
          Object.entries(value).map(async ([key, item]) => [
            key,
            await encode(item),
          ]),
        ),
      ];
    // Unknown structured values (including non-exportable keys) must stop the
    // migration rather than silently producing an incomplete backup.
    throw new Error(
      `Unsupported saved value: ${Object.prototype.toString.call(value)}`,
    );
  }
  const request = (value) =>
    new Promise((resolve, reject) => {
      let blocked = false;
      value.onsuccess = () => {
        if (blocked) {
          value.result?.close?.();
          return;
        }
        resolve(value.result);
      };
      value.onerror = () => reject(value.error);
      if (capture)
        value.onblocked = () => {
          blocked = true;
          reject(
            new Error(
              "The saved browser source is blocked. Close older source views before migration.",
            ),
          );
        };
    });
  checkCapture();
  const preferences = Object.fromEntries(
    Array.from({ length: localStorage.length }, (_, index) => {
      const key = localStorage.key(index);
      return [key, localStorage.getItem(key)];
    }),
  );
  const databases = [];
  for (const { name, version } of await indexedDB.databases()) {
    checkCapture();
    const opened = indexedDB.open(name, version);
    opened.onupgradeneeded = () => opened.transaction.abort();
    const db = await request(opened);
    try {
      const names = [...db.objectStoreNames];
      if (!names.length) {
        databases.push({ name, version, stores: [] });
        continue;
      }
      const transaction = db.transaction(names, "readonly");
      const completed = new Promise((resolve, reject) => {
        transaction.oncomplete = resolve;
        transaction.onerror = transaction.onabort = () =>
          reject(transaction.error || new Error("Storage read aborted"));
      });
      // A schema rejection below still observes a possible transaction error.
      void completed.catch(() => {});
      const pending = names.map((name) => {
        const store = transaction.objectStore(name);
        return {
          name,
          keyPath: store.keyPath,
          autoIncrement: store.autoIncrement,
          indexes: [...store.indexNames].map((name) => {
            const index = store.index(name);
            return {
              name,
              keyPath: index.keyPath,
              unique: index.unique,
              multiEntry: index.multiEntry,
            };
          }),
        };
      });
      const stores = pending;
      await completed;
      // Observe the generator in an aborted transaction. Neither the probe nor
      // its increment is committed; deleted high keys must still be preserved.
      for (const store of stores) {
        if (!store.autoIncrement) continue;
        if (capture) {
          store.generatorUnobserved = true;
          continue;
        }
        store.nextKey = await new Promise((resolve, reject) => {
          const probe = db.transaction(store.name, "readwrite");
          let key;
          let failure;
          probe.onabort = () => (failure ? reject(failure) : resolve(key));
          probe.oncomplete = () =>
            reject(new Error("Key probe unexpectedly committed"));
          const added = probe.objectStore(store.name).add({});
          added.onsuccess = () => {
            key = added.result;
            probe.abort();
          };
          added.onerror = () => {
            failure = added.error;
          };
        });
      }
      for (const store of stores) {
        const records = [];
        const chunks = [];
        let chunkBytes = 2;
        let lower;
        let more = true;
        const flush = async () => {
          if (!records.length || !emitChunk) return;
          chunks.push(await emitChunk(records));
          records.length = 0;
          chunkBytes = 2;
        };
        while (more) {
          checkCapture();
          // Each bounded batch completes before Blob decoding or disk I/O.
          // The offline reader is the only process that opens this profile.
          const tx = db.transaction(store.name, "readonly");
          const done = new Promise((resolve, reject) => {
            tx.oncomplete = resolve;
            tx.onerror = tx.onabort = () =>
              reject(tx.error || new Error("Storage read aborted"));
          });
          void done.catch(() => {});
          const source = tx.objectStore(store.name);
          const range =
            lower === undefined
              ? undefined
              : IDBKeyRange.lowerBound(lower, true);
          const [keys, values] = await Promise.all([
            request(source.getAllKeys(range, 32)),
            request(source.getAll(range, 32)),
          ]);
          await done;
          checkCapture();
          more = keys.length === 32;
          for (let index = 0; index < keys.length; index++) {
            reservedRecordBytes = 0;
            const record = [
              await encode(keys[index]),
              await encode(values[index]),
            ];
            const size =
              new TextEncoder().encode(JSON.stringify(record)).length + 1;
            capturedBytes += size;
            if (capture && capturedBytes > capture.maxRecordBytes)
              throw new Error(
                "The native source capture exceeds its record bound. The source was preserved.",
              );
            if (emitChunk && size > 64 * 1024 * 1024 - 2)
              throw new Error(
                "A saved record exceeds the 64 MiB migration chunk limit.",
              );
            if (emitChunk && chunkBytes + size > 8 * 1024 * 1024) await flush();
            records.push(record);
            chunkBytes += size;
          }
          if (keys.length) lower = keys.at(-1);
        }
        await flush();
        if (emitChunk) store.chunks = chunks;
        else store.records = records;
      }
      databases.push({ name, version, stores });
    } finally {
      db.close();
    }
  }
  checkCapture();
  return {
    version: emitChunk ? 2 : 1,
    origin: location.origin,
    preferences,
    databases,
  };
}
