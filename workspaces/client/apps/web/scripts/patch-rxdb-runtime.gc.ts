import assert from "node:assert/strict";
import { createRxDatabase } from "rxdb";
import { getRxStorageMemory } from "rxdb/plugins/storage-memory";
import type {
  RxChangeEventBulk,
  RxDocumentData,
  RxCollection,
  RxStorageInstance,
} from "rxdb";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

// RxDB 17.6.0 retains completed event batches and payloads in a strong Map.
// A WeakMap preserves identity reuse while allowing those objects to be collected.
// Projection pulls must retain 409 conflicts for server-sequence comparison;
// RxDB's automatic deleted-document reinsertion remains enabled elsewhere.
// A rejected incremental storage write (for example, a full disk) must reject
// its callers and release the queue; this behavior is now fixed upstream.
// Recheck these runtime changes whenever the pinned RxDB version changes.
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
const failures: string[] = [];
// Unpatched RxDB leaves a rejected incremental write as an unhandled rejection.
process.on("unhandledRejection", (error) => {
  failures.push(
    `unhandled rejection: ${error instanceof Error ? error.stack : error}`,
  );
});

async function testEventCache(
  format: string,
  implementation: Pick<
    typeof import("rxdb"),
    "rxChangeEventBulkToRxChangeEvents"
  >,
) {
  function makeBatch() {
    const payload: RxDocumentData<{ payload: string }> = {
      payload: "x".repeat(2 * 1024 * 1024),
      _deleted: false,
      _attachments: {},
      _meta: { lwt: 1 },
      _rev: "1-a",
    };
    const bulk: RxChangeEventBulk<{ payload: string }> = {
      id: "fixture",
      checkpoint: { id: "a", lwt: 1 },
      context: "fixture",
      databaseToken: "fixture",
      storageToken: "fixture",
      internal: false,
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
    assert.ok(global.gc);
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

type WitnessDocument = { id: string; value: number };
async function databaseFixture(format: string) {
  const database = await createRxDatabase({
    name: "gcwitness" + format + Math.random().toString(36).slice(2),
    storage: getRxStorageMemory(),
    multiInstance: false,
  });
  const collections = await database.addCollections<{
    projections: RxCollection<WitnessDocument>;
  }>({
    projections: {
      schema: {
        version: 0,
        primaryKey: "id",
        type: "object",
        properties: {
          id: { type: "string", maxLength: 100 },
          value: { type: "number" },
        },
        required: ["id", "value"],
        additionalProperties: false,
      },
    },
  });
  const storageInstance: RxStorageInstance<WitnessDocument, unknown, unknown> =
    collections.projections.storageInstance;
  return { database, storageInstance };
}
const document: RxDocumentData<WitnessDocument> = {
  id: "deleted-row",
  value: 0,
  _deleted: false,
  _attachments: {},
  _meta: { lwt: 1 },
  _rev: "1-a",
};
async function testProjectionConflict(format: string) {
  const helperFile = `${rxdbRoot}/dist/${format}/rx-storage-helper.js`;
  const helper: Pick<typeof import("rxdb"), "getWrappedStorageInstance"> =
    format === "esm"
      ? await import(pathToFileURL(helperFile).href)
      : require(helperFile);
  const { database, storageInstance } = await databaseFixture(format);
  const callCounts = new Map<string, number>();
  storageInstance.bulkWrite = async (rows, context) => {
    callCounts.set(context, (callCounts.get(context) ?? 0) + 1);
    return {
      error: rows.map((writeRow) => ({
        status: 409,
        isError: true,
        context,
        documentId: writeRow.document.id,
        writeRow,
        documentInDb: {
          ...writeRow.document,
          _deleted: true,
          _meta: { lwt: 1 },
          _rev: "1-existing",
        },
      })),
    };
  };
  try {
    const wrapped = helper.getWrappedStorageInstance(
      database,
      storageInstance,
      storageInstance.schema,
    );
    const write = { document: { ...document } };
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
  } finally {
    await database.close();
  }
}

async function testIncrementalWriteFailure(format: string) {
  const queueFile = `${rxdbRoot}/dist/${format}/incremental-write.js`;
  const {
    IncrementalWriteQueue,
  }: typeof import("../node_modules/rxdb/dist/types/incremental-write.js") =
    format === "esm"
      ? await import(pathToFileURL(queueFile).href)
      : require(queueFile);
  let failNext = true;
  const { database, storageInstance } = await databaseFixture(format);
  storageInstance.bulkWrite = async () => {
    if (failNext) {
      failNext = false;
      throw new Error("disk full");
    }
    return { error: [] };
  };
  try {
    const queue = new IncrementalWriteQueue<WitnessDocument>(
      storageInstance,
      "id",
      async () => {},
      () => {},
    );
    const state: RxDocumentData<WitnessDocument> = {
      ...document,
      id: "outbox-row",
    };
    const settle = <T>(
      work: Promise<T>,
    ): Promise<{ value?: T; error?: unknown; pending?: boolean }> =>
      Promise.race([
        work.then(
          (value) => ({ value }),
          (error) => ({ error }),
        ),
        delay().then(() => ({ pending: true })),
      ]);
    const failed = await settle(
      queue.addWrite(state, async (doc) => ({ ...doc, value: 1 })),
    );
    assert.equal(
      failed.error instanceof Error ? failed.error.message : undefined,
      "disk full",
      `${format}: a rejected storage write rejects its caller`,
    );
    const next = await settle(
      queue.addWrite(state, async (doc) => ({ ...doc, value: 2 })),
    );
    assert.equal(
      next.value?.value,
      2,
      `${format}: the next incremental write runs after a rejected write`,
    );
  } finally {
    await database.close();
  }
}

for (const format of ["esm", "cjs"]) {
  const eventFile = `${rxdbRoot}/dist/${format}/rx-change-event.js`;
  const implementation: Pick<
    typeof import("rxdb"),
    "rxChangeEventBulkToRxChangeEvents"
  > =
    format === "esm"
      ? await import(pathToFileURL(eventFile).href)
      : require(eventFile);
  for (const [name, verify] of [
    ["event cache", () => testEventCache(format, implementation)],
    ["projection conflict", () => testProjectionConflict(format)],
    ["incremental write failure", () => testIncrementalWriteFailure(format)],
  ] as const) {
    try {
      await verify();
    } catch (error) {
      failures.push(
        `${format} ${name}: ${error instanceof Error ? error.stack : String(error)}`,
      );
    }
  }
}

if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exitCode = 1;
} else {
  console.log(
    "PASS: RxDB ESM/CJS caches release payloads, projection conflicts remain visible, and failed incremental writes release the queue",
  );
}
