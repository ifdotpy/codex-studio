import { hiddenElectronArguments } from "./hidden-electron.ts";
import type { ProfileRestoreFixture } from "./profile-export-fixture.ts";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import {
  mkdtemp,
  writeFile,
  readFile,
  stat,
  readdir,
  open,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import type { BrowserProfileArchive } from "../web/src/native/importBrowserProfile.ts";
import type { BrowserProfileRecord } from "../web/src/native/importBrowserProfile.ts";
const electron: unknown = createRequire(import.meta.url)("electron");
assert.ok(typeof electron === "string");
// The exporter in this process writes this archive; this boundary reads its own fixture output.
function readArchive(text: string): BrowserProfileArchive {
  return JSON.parse(text);
}
// Chunk bytes below are the paired exporter fixture output.
function readRecords(text: string): BrowserProfileRecord[] {
  return JSON.parse(text);
}
import { combineBrowserProfiles } from "../web/src/native/combineBrowserProfiles.ts";

const run = promisify(execFile);
const fixture = await mkdtemp(path.join(tmpdir(), "studio-profile-export-"));
const profile = path.join(fixture, "profile");
const output = path.join(fixture, "snapshot.json");
const entry = path.join(import.meta.dirname, "profile-export-fixture.ts");
const manifest = path.join(fixture, "manifest.json");
await writeFile(
  manifest,
  JSON.stringify({
    profile,
    output,
    origins: ["http://127.0.0.1:4620", "http://studio-nrxwgylm.localhost:4623"],
    contexts: {
      "http://studio-nrxwgylm.localhost:4623": "http://127.0.0.1:4620",
    },
  }),
);
await run(electron, [entry, "seed", manifest, ...hiddenElectronArguments], {
  timeout: 90000,
});
const exporter = path.join(import.meta.dirname, "export-profile.ts");
const heldProfile = await open(path.join(profile, "Preferences"), "r");
try {
  await assert.rejects(
    run(electron, [exporter, manifest, ...hiddenElectronArguments], {
      timeout: 90000,
    }),
    /Close the desktop/,
  );
} finally {
  await heldProfile.close();
}
const exported = await run(
  electron,
  [exporter, manifest, ...hiddenElectronArguments],
  { timeout: 90000 },
);
assert.match(
  exported.stdout,
  /"result":"PASS"/,
  `Exporter did not finish: ${exported.stderr}`,
);
const archive = readArchive(await readFile(output, "utf8"));
assert.equal(archive.version, 2);
assert.equal(archive.snapshots.length, 2);
assert.ok(archive.snapshots[1]);
assert.equal(archive.snapshots[1].topLevelOrigin, "http://127.0.0.1:4620");
assert.equal(
  archive.snapshots[1].databases[0]?.stores[0]?.chunks?.[0]?.records,
  1,
);
assert.deepEqual(archive.snapshots[1].preferences, {
  "frame-only": "partitioned",
});
const snapshot = combineBrowserProfiles(
  archive.snapshots.map((snapshot, index) => ({
    owner: index === 0 ? "root" : "server:remote",
    snapshot,
  })),
);
assert.equal(snapshot.preferences["unexpected-app-run"], undefined);
assert.equal(
  // The seed writes this exact preference immediately before export.
  (
    JSON.parse(
      snapshot.preferences["codex-drafts:workspace"] ?? "null",
    ) as Record<string, unknown>
  ).room,
  "Сохранить 🦀",
);
const store = snapshot.databases
  .find((db) => db.name === "studio-fixture")
  ?.stores.find((store) => store.name === "records");
assert.ok(store);
assert.equal(
  snapshot.databases
    .find((db) => db.name === "studio-fixture")
    ?.stores.find((store) => store.name === "changes")?.nextKey,
  101,
);
assert.equal(
  snapshot.databases
    .find((db) => db.name === "studio-fixture")
    ?.stores.find((store) => store.name === "nested")?.nextKey,
  51,
);
assert.equal(store.keyPath, "id");
assert.ok(store.chunks);
assert.ok(store.chunks.length > 1);
assert.equal(
  store.chunks.reduce((sum, chunk) => sum + chunk.records, 0),
  71,
);
assert.deepEqual(store.indexes, [
  { name: "session", keyPath: "session", unique: false, multiEntry: false },
]);
const chunks: Record<string, BrowserProfileRecord[]> = {};
const materialized = structuredClone(snapshot.databases);
for (const db of materialized)
  for (const savedStore of db.stores) {
    savedStore.records = [];
    assert.ok(savedStore.chunks);
    for (const chunk of savedStore.chunks) {
      const chunkPath = path.join(
        fixture,
        archive.parts ?? "",
        chunk.sha256 + ".json",
      );
      assert.equal((await stat(chunkPath)).mode & 0o777, 0o600);
      const bytes = await readFile(chunkPath);
      assert.equal(bytes.length, chunk.bytes);
      chunks[chunk.sha256] = readRecords(bytes.toString());
      savedStore.records.push(...readRecords(bytes.toString()));
    }
    delete savedStore.chunks;
  }
const records = materialized
  .find((db) => db.name === "studio-fixture")
  ?.stores.find((s) => s.name === "records");
assert.ok(records?.records?.[0]);
assert.equal(records.records[0][1][0], "object");
const encoded = records.records[0][1];
assert.ok(encoded[0] === "object");
const value = Object.fromEntries(encoded[1]);
assert.deepEqual(value.pcm, ["view", "Int16Array", ["bytes", "AIAAAP9/"]]);
assert.deepEqual(value.file, [
  "file",
  "bytes.bin",
  "application/octet-stream",
  1234,
  ["bytes", "AP8q"],
]);
assert.deepEqual(value.optional, ["undefined"]);
assert.equal((await stat(output)).mode & 0o777, 0o600);
await assert.rejects(
  run(electron, [exporter, manifest, ...hiddenElectronArguments], {
    timeout: 90000,
  }),
  /EEXIST/,
);
assert.equal(await readFile(output, "utf8"), JSON.stringify(archive));
assert.ok(!(await readdir(fixture)).some((name) => name.endsWith(".tmp")));
const restoreDocument = path.join(fixture, "restore.json");
await writeFile(
  restoreDocument,
  JSON.stringify({
    profile: path.join(fixture, "destination"),
    snapshot,
    chunks,
    materialized,
  } satisfies ProfileRestoreFixture),
);
await run(
  electron,
  [entry, "restore", restoreDocument, ...hiddenElectronArguments],
  { timeout: 90000 },
);
console.log(
  JSON.stringify({
    result: "PASS",
    fixture,
    checks:
      "profile round trip preserves preferences, IndexedDB schema, PCM and files; interrupted import resumes; completed retry preserves edits; conflicts and archive replacement rejected",
  }),
);
