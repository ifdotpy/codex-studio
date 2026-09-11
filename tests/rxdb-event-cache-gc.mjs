import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { mkdtemp, mkdir, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
if (!global.gc) {
  const result = spawnSync(
    process.execPath,
    ["--expose-gc", fileURLToPath(import.meta.url)],
    { stdio: "inherit" },
  );
  process.exit(result.status ?? 1);
}
const { patchRxdb } = require("../web/scripts/patch-rxdb-runtime.cjs");
const root = dirname(require.resolve("rxdb/package.json"));
patchRxdb(root);
assert.equal(patchRxdb(root), false, "The patch is idempotent");
const temporary = await mkdtemp(join(tmpdir(), "studio-rxdb-patch-"));
await writeFile(
  join(temporary, "package.json"),
  JSON.stringify({ version: "99.0.0" }),
);
assert.throws(
  () => patchRxdb(temporary),
  /Review the RxDB/,
  "Unknown versions cannot silently ship",
);
await writeFile(
  join(temporary, "package.json"),
  JSON.stringify({ version: "17.5.0" }),
);
for (const format of ["esm", "cjs"]) {
  await mkdir(join(temporary, "dist", format), { recursive: true });
  await writeFile(
    join(temporary, "dist", format, "rx-storage-helper.js"),
    "if (error.status === 409 && !error.writeRow.previous &&",
  );
  await writeFile(
    join(temporary, "dist", format, "rx-change-event.js"),
    format === "esm"
      ? "var EVENT_BULK_CACHE = new Map();"
      : "unexpected source",
  );
}
assert.throws(() => patchRxdb(temporary), /Unexpected RxDB/);
assert.equal(
  await readFile(join(temporary, "dist/esm/rx-change-event.js"), "utf8"),
  "var EVENT_BULK_CACHE = new Map();",
  "Validation precedes all writes",
);
const delay = () => new Promise((resolve) => setTimeout(resolve, 10));
await rm(temporary, { recursive: true, force: true });
for (const format of ["esm", "cjs"]) {
  const file = join(root, "dist", format, "rx-change-event.js");
  const mod =
    format === "esm" ? await import(pathToFileURL(file)) : require(file);
  const original = (await readFile(file, "utf8")).replace(
    "var EVENT_BULK_CACHE = new WeakMap();",
    "var EVENT_BULK_CACHE = new Map();",
  );
  let negative;
  if (format === "esm") {
    const resolved = original.replace(
      /from (["'])(\.[^"']+)\1/g,
      (_all, quote, relative) =>
        `from ${quote}${pathToFileURL(join(dirname(file), relative))}${quote}`,
    );
    negative = await import(
      `data:text/javascript;base64,${Buffer.from(resolved).toString("base64")}`
    );
  } else {
    const module = { exports: {} };
    new Function("require", "module", "exports", original)(
      createRequire(pathToFileURL(file)),
      module,
      module.exports,
    );
    negative = module.exports;
  }
  for (const [implementation, shouldCollect] of [
    [mod, true],
    [negative, false],
  ]) {
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
        "Live batches keep cached identity",
      );
      return [new WeakRef(bulk), new WeakRef(payload)];
    }
    const refs = makeBatch();
    let released = false;
    for (let i = 0; i < (shouldCollect ? 100 : 8); i++) {
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
      shouldCollect,
      `${format}: patched cache collects payloads; original Map retains them`,
    );
  }
}
console.log(
  "PASS: RxDB ESM/CJS event cache releases completed payloads; patch is idempotent and fails closed",
);
