import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

if (!global.gc) {
  const result = spawnSync(
    process.execPath,
    ["--expose-gc", fileURLToPath(import.meta.url)],
    { stdio: "inherit" },
  );
  process.exit(result.status ?? 1);
}

const require = createRequire(new URL("../package.json", import.meta.url));
const rxdbRoot = dirname(require.resolve("rxdb/package.json"));
const delay = () => new Promise((resolve) => setTimeout(resolve, 10));

for (const format of ["esm", "cjs"]) {
  const eventFile = `${rxdbRoot}/dist/${format}/rx-change-event.js`;
  const implementation =
    format === "esm"
      ? await import(pathToFileURL(eventFile))
      : require(eventFile);

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
    `${format}: patched cache releases completed event batches and payloads`,
  );
}

console.log("PASS: RxDB ESM/CJS event cache releases completed payloads");
