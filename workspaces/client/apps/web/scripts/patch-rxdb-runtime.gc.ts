import type {
  RxChangeEventBulk,
  RxDocumentData,
  RxStorageInstance,
  RxJsonSchema,
} from "rxdb";
type WitnessDocument = { id: string; value: number };
import assert from "node:assert/strict";
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
  failures.push(`unhandled rejection: ${(error as Error)?.stack ?? error}`);
});

async function testEventCache(
  format: string,
  implementation: Pick<
    typeof import("rxdb"),
    "rxChangeEventBulkToRxChangeEvents"
  >,
) {
  function makeBatch() {
    // Cache identity needs no storage metadata (_rev, _meta, _deleted, _attachments).
    const payload = { payload: "x".repeat(2 * 1024 * 1024) } as RxDocumentData<{
      payload: string;
    }>;
    // The cache converter ignores batch ids, checkpoint, context and storage tokens.
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
    } as RxChangeEventBulk<{ payload: string }>;
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
    global.gc!();
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

async function testProjectionConflict(format: string) {
  const helperFile = `${rxdbRoot}/dist/${format}/rx-storage-helper.js`;
  const helper: Pick<typeof import("rxdb"), "getWrappedStorageInstance"> =
    format === "esm"
      ? await import(pathToFileURL(helperFile).href)
      : require(helperFile);
  // The wrapper only reads the primary key; validation and indexes are unused here.
  const schema = { primaryKey: "id" } as RxJsonSchema<
    RxDocumentData<WitnessDocument>
  >;
  const callCounts = new Map<string, number>();
  // The wrapper uses schema and bulkWrite; storage lifecycle methods are unused.
  const storageInstance = {
    schema,
    internals: {},
    collectionName: "projections",
    databaseName: "studio",
    options: {},
    async bulkWrite(
      rows: Parameters<
        RxStorageInstance<WitnessDocument, unknown, unknown>["bulkWrite"]
      >[0],
      context: string,
    ) {
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
  } as RxStorageInstance<WitnessDocument, unknown, unknown>;
  // The wrapper uses the token, storage set and lock; database lifecycle methods are unused.
  const database = {
    token: "test-token",
    storageInstances: new Set(),
    lockedRun: <T>(operation: () => T) => operation(),
  } as Parameters<typeof helper.getWrappedStorageInstance>[0];
  const wrapped = helper.getWrappedStorageInstance(
    database,
    storageInstance,
    schema,
  );
  // Storage metadata and value are unused by the forced-conflict write.
  const write = {
    document: { id: "deleted-row", _deleted: false },
    previous: undefined,
  } as Parameters<typeof wrapped.bulkWrite>[0][number];
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

async function testIncrementalWriteFailure(format: string) {
  const queueFile = `${rxdbRoot}/dist/${format}/incremental-write.js`;
  const {
    IncrementalWriteQueue,
  }: typeof import("../node_modules/rxdb/dist/types/incremental-write.js") =
    format === "esm"
      ? await import(pathToFileURL(queueFile).href)
      : require(queueFile);
  let failNext = true;
  // The queue calls bulkWrite only; storage schema and lifecycle methods are unused.
  const storageInstance = {
    async bulkWrite(
      rows: Parameters<
        RxStorageInstance<WitnessDocument, unknown, unknown>["bulkWrite"]
      >[0],
    ) {
      if (failNext) {
        failNext = false;
        throw new Error("disk full");
      }
      return { error: [], success: rows.map((row) => row.document) };
    },
  };
  const queue = new IncrementalWriteQueue<WitnessDocument>(
    storageInstance as typeof storageInstance &
      RxStorageInstance<WitnessDocument, unknown, unknown>,
    "id",
    async () => {},
    () => {},
  );
  // This queue witness does not use _meta, _deleted or attachments.
  const state = {
    id: "outbox-row",
    value: 0,
    _rev: "1-a",
  } as RxDocumentData<WitnessDocument>;
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
    (failed.error as Error | undefined)?.message,
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
      failures.push(`${format} ${name}: ${(error as Error).stack}`);
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
