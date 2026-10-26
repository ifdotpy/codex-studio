import assert from "node:assert/strict";
import { File } from "node:buffer";
import { createRequire } from "node:module";
import { test } from "node:test";
import vm from "node:vm";
import { exportBrowserProfile } from "./legacyProfileReader.mjs";
import { exportBrowserProfile as desktopReader } from "../apps/desktop/export-browser-profile.mjs";

const require = createRequire(import.meta.url);
const policy = require("../contracts/native-migration.json");
function fixture(preferences = {}) {
  const saved = [];
  const modes = [];
  const db = {
    objectStoreNames: ["records"],
    close() {},
    transaction(_names, mode) {
      modes.push(mode);
      assert.equal(mode, "readonly");
      let pending = 0;
      const transaction = {
        objectStore() {
          function request(result) {
            pending++;
            const request = {};
            queueMicrotask(() => {
              request.result = result;
              request.onsuccess?.();
              if (--pending === 0)
                queueMicrotask(() => transaction.oncomplete?.());
            });
            return request;
          }
          const rows = (range, limit) =>
            saved
              .filter((row) => !range || row.key > range.lower)
              .slice(0, limit);
          return {
            keyPath: null,
            autoIncrement: true,
            indexNames: [],
            getAllKeys: (range, limit) =>
              request(rows(range, limit).map((row) => row.key)),
            getAll: (range, limit) =>
              request(rows(range, limit).map((row) => row.value)),
          };
        },
      };
      queueMicrotask(() => {
        if (!pending) transaction.oncomplete?.();
      });
      return transaction;
    },
  };
  const indexedDB = {
    databases: async () =>
      saved.length ? [{ name: "saved", version: 1 }] : [],
    open() {
      const request = {};
      queueMicrotask(() => {
        request.result = db;
        request.onsuccess?.();
      });
      return request;
    },
  };
  const IDBKeyRange = { lowerBound: (lower) => ({ lower }) };
  const values = new Map(Object.entries(preferences));
  const writes = [];
  const localStorage = {
    get length() {
      return values.size;
    },
    key: (index) => [...values.keys()][index] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem(...args) {
      writes.push(args);
      throw new Error("source write");
    },
  };
  const world = {
    indexedDB,
    IDBKeyRange,
    localStorage,
    location: { protocol: "tauri:", host: "localhost", origin: "null" },
    Object,
    Array,
    Date,
    Number,
    String,
    JSON,
    Promise,
    Error,
    ArrayBuffer,
    Uint8Array,
    Int16Array,
    File,
    Blob,
    TextEncoder,
    btoa,
  };
  const capture = {
    readOnly: true,
    expectedOrigin: "tauri://localhost",
    maxRecordBytes: policy.captureRecordBytes,
  };
  const run = (options = capture) =>
    vm
      .runInNewContext(`(${exportBrowserProfile.toString()})(null, capture)`, {
        ...world,
        capture: options,
      })
      .then((source) => JSON.parse(JSON.stringify(source)));
  return { indexedDB, saved, modes, values, writes, run, capture };
}

test("native legacy reader returns exact source values without source writes", async () => {
  assert.equal(desktopReader, exportBrowserProfile);
  const f = fixture({ theme: "saved", "request-id:old": "original" });
  const source = await f.run();
  assert.deepEqual(source.preferences, {
    theme: "saved",
    "request-id:old": "original",
  });
  assert.deepEqual(source.databases, []);
  assert.deepEqual(f.writes, []);
  await assert.rejects(
    f.run({ ...f.capture, expectedOrigin: "studio-view://other.localhost" }),
    /identity/,
  );
  await assert.rejects(
    f.run({ ...f.capture, isCancelled: () => true }),
    /canceled/,
  );
});

test("native legacy reader preserves tagged bytes and does not probe auto-increment with a write", async () => {
  const f = fixture();
  f.saved.push({
    key: 50,
    value: {
      id: "old",
      pcm: new Int16Array([-32768, 0, 32767]),
      file: new File([new Uint8Array([0, 255])], "old.bin"),
    },
  });
  const source = await f.run();
  const store = source.databases[0].stores[0];
  assert.equal(store.generatorUnobserved, true);
  assert.equal(store.nextKey, undefined);
  assert.deepEqual(store.records[0][0], ["value", 50]);
  assert.match(JSON.stringify(store.records[0][1]), /Int16Array/);
  assert.equal(
    f.modes.every((mode) => mode === "readonly"),
    true,
  );
  await assert.rejects(f.run({ ...f.capture, maxRecordBytes: 1 }), /bound/);
});

test("native legacy reader refuses oversized Blob and File metadata before reading their bytes", async () => {
  for (const base of [Blob, File]) {
    let reads = 0;
    class Oversized extends base {
      get size() {
        return policy.captureRecordBytes + 1;
      }
      async arrayBuffer() {
        reads++;
        return new ArrayBuffer(0);
      }
    }
    // Only the size metadata is large. This fixture allocates no large buffer.
    const f = fixture();
    const value =
      base === File ? new Oversized([], "oversized.bin") : new Oversized([]);
    f.saved.push({ key: 1, value });
    await assert.rejects(f.run(), /bound/);
    assert.equal(reads, 0);
    assert.deepEqual(f.writes, []);
  }
});

test("native legacy reader reserves the aggregate binary budget before concurrent Blob reads", async () => {
  let reads = 0;
  class CountedBlob extends Blob {
    async arrayBuffer() {
      reads++;
      return super.arrayBuffer();
    }
  }
  const f = fixture();
  f.saved.push({
    key: 1,
    value: [
      new CountedBlob([new Uint8Array(40)]),
      new CountedBlob([new Uint8Array(40)]),
    ],
  });
  await assert.rejects(f.run({ ...f.capture, maxRecordBytes: 100 }), /bound/);
  assert.equal(
    reads,
    1,
    "The second binary read must be refused before its allocation",
  );
  assert.deepEqual(f.writes, []);
});
