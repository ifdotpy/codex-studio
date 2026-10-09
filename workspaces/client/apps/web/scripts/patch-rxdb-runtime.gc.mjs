import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

// RxDB 17.5.0 retains completed event batches and payloads in a strong Map.
// A WeakMap preserves identity reuse while allowing those objects to be collected.
// Projection pulls must retain 409 conflicts for server-sequence comparison;
// RxDB's automatic deleted-document reinsertion remains enabled elsewhere.
// Recheck both runtime changes whenever the pinned RxDB version changes.
if (!global.gc) {
  const result = spawnSync(
    process.execPath,
    ["--expose-gc", fileURLToPath(import.meta.url)],
    { stdio: "inherit" },
  );
  process.exit(result.status ?? 1);
}

const require = createRequire(new URL("../package.json", import.meta.url));
const rxdbRoot =
  process.env.RXDB_TEST_ROOT ?? dirname(require.resolve("rxdb/package.json"));
const delay = () => new Promise((resolve) => setTimeout(resolve, 10));
const failures = [];

async function testEventCache(format, implementation) {
  function makeBatch() {
    const payload = { payload: "x".repeat(2 * 1024 * 1024) };
    const bulk = {
      collectionName: "projections",
      isLocal: false,
      events: [
        {
          documentId: "a",
          operation: "UPDATE",
          documentData: payload,
          previousDocumentData: payload,
        },
      ],
    };
    const first = implementation.rxChangeEventBulkToRxChangeEvents(bulk);
    assert.equal(
      implementation.rxChangeEventBulkToRxChangeEvents(bulk),
      first,
      `${format}: live batches keep cached identity`,
    );
    return [new WeakRef(bulk), new WeakRef(payload)];
  }

  const refs = makeBatch();
  let released = false;
  for (let i = 0; i < 100; i++) {
    await delay();
    global.gc();
    await delay();
    if (refs.every((ref) => ref.deref() === undefined)) {
      released = true;
      break;
    }
  }
  assert.equal(
    released,
    true,
    `${format}: event cache releases completed batches and payloads`,
  );
}

async function testProjectionConflict(format) {
  const helperFile = `${rxdbRoot}/dist/${format}/rx-storage-helper.js`;
  const helper =
    format === "esm"
      ? await import(pathToFileURL(helperFile))
      : require(helperFile);
  const schema = { primaryKey: "id" };
  const callCounts = new Map();
  const storageInstance = {
    schema,
    internals: {},
    collectionName: "projections",
    databaseName: "studio",
    options: {},
    async bulkWrite(rows, context) {
      callCounts.set(context, (callCounts.get(context) ?? 0) + 1);
      return {
        error: [
          {
            status: 409,
            documentId: "deleted-row",
            writeRow: rows[0],
            documentInDb: {
              _deleted: true,
              _meta: { lwt: 1 },
              _rev: "1-existing",
            },
          },
        ],
      };
    },
  };
  const database = {
    token: "test-token",
    storageInstances: new Set(),
    lockedRun: (operation) => operation(),
  };
  const wrapped = helper.getWrappedStorageInstance(
    database,
    storageInstance,
    schema,
  );
  const write = {
    document: { id: "deleted-row", _deleted: false },
    previous: undefined,
  };
  const projectionResult = await wrapped.bulkWrite(
    [write],
    "studio-projection-pull",
  );
  assert.equal(projectionResult.error.length, 1);
  assert.equal(
    callCounts.get("studio-projection-pull"),
    1,
    `${format}: projection 409 remains visible without an automatic retry`,
  );

  await wrapped.bulkWrite(
    [{ ...write, document: { ...write.document } }],
    "replication",
  );
  assert.equal(
    callCounts.get("replication"),
    2,
    `${format}: automatic deleted-document reinsertion remains enabled elsewhere`,
  );
}

for (const format of ["esm", "cjs"]) {
  const eventFile = `${rxdbRoot}/dist/${format}/rx-change-event.js`;
  const implementation =
    format === "esm"
      ? await import(pathToFileURL(eventFile))
      : require(eventFile);
  for (const [name, verify] of [
    ["event cache", () => testEventCache(format, implementation)],
    ["projection conflict", () => testProjectionConflict(format)],
  ]) {
    try {
      await verify();
    } catch (error) {
      failures.push(`${format} ${name}: ${error.stack}`);
    }
  }
}

if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exitCode = 1;
} else {
  console.log(
    "PASS: RxDB ESM/CJS caches release payloads and projection conflicts remain visible",
  );
}
