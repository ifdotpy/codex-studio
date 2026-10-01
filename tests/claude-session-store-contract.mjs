import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createSessionStore } from "../scripts/claude_bridge/session-store.mjs";

const id = "11111111-1111-4111-8111-111111111111";
const makeSession = () => ({
  id,
  cwd: "/tmp/work",
  turns: [{ id: "turn-1", status: "completed", items: [{ id: "user-1", content: [{ type: "text", text: "hello" }] }] }],
});
const temporary = async (run) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-session-store-"));
  try { await run(root); }
  finally { await fs.rm(root, { recursive: true, force: true }); }
};

await temporary(async (root) => {
  const store = createSessionStore(root);
  const original = makeSession();
  store.sessions.set(id, original);
  await store.persist(original);
  original.turns[0].items.push({ id: "answer-1", text: "saved" });
  await store.persist(original);
  await store.evict(id);
  assert.equal(store.sessions.has(id), false, "settled idle session leaves the cache");
  const restored = await store.get(id);
  assert.deepEqual(restored, original, "session history and request identity reload exactly");
  assert.equal(store.stats().pendingWrites, 0, "settled write promise is removed");
  const firstMetadata = await store.metadata(id);
  assert.equal(firstMetadata.id, id);
  assert.equal(typeof firstMetadata.revision, "string");
  assert.equal("turns" in firstMetadata, false, "sidecar metadata excludes transcript bodies");
  assert.deepEqual(await store.listMetadata(null, 1), {
    data: [firstMetadata], nextCursor: null,
  });
});

await temporary(async (root) => {
  const store = createSessionStore(root);
  const original = makeSession();
  store.sessions.set(id, original);
  await store.persist(original);
  const before = await store.metadata(id);
  original.turns[0].items.push({ id: "changed", text: "history changed" });
  await store.persist(original);
  const after = await store.metadata(id);
  assert.notEqual(after.revision, before.revision, "persist updates the stored history revision");
  assert.equal((await store.metadata(id)).revision, after.revision,
    "metadata reads reuse the stored revision without serializing turns");
});

await temporary(async (root) => {
  const store = createSessionStore(root);
  const original = makeSession();
  store.sessions.set(id, original);
  await store.persist(original);
  const heldByHandler = await store.get(id);
  await store.evict(id);
  const loading = store.get(id);
  heldByHandler.turns[0].items.push({ id: "during-load", text: "written by the held object" });
  const saving = store.persist(heldByHandler);
  const reloaded = await loading;
  await saving;
  assert.strictEqual(reloaded, heldByHandler, "weak reference reuses an object held across an await");
  heldByHandler.turns[0].items.push({ id: "after-race", text: "current state" });
  await store.persist(heldByHandler);
  assert.equal(store.stats().retainedBytes, Buffer.byteLength(JSON.stringify(heldByHandler)) * 2,
    "incremental cache bytes track the live session and shadow");
  assert.deepEqual((await createSessionStore(root).readAll(id)).session, heldByHandler,
    "persist does not journal a stale copy over concurrent state");
  await assert.rejects(
    store.persist(structuredClone(heldByHandler)),
    /not the cached identity/,
    "a different object with the same session id cannot replace a live cached object",
  );
});

await temporary(async (root) => {
  const store = createSessionStore(root, { compactRecords: 3, compactBytes: 1024 * 1024 });
  const original = makeSession();
  original.turns[0].items.push({ id: "prior-history", result: { content: "h".repeat(4096) } });
  store.sessions.set(id, original);
  await store.persist(original);
  const snapshotPath = path.join(root, "sessions", id + ".json");
  const journalPath = path.join(root, "sessions", id + ".jsonl");
  const firstSnapshot = await fs.readFile(snapshotPath, "utf8");
  original.turns[0].items.push({ id: "frame-1", text: "small update" });
  await store.persist(original);
  const firstJournal = await fs.readFile(journalPath, "utf8");
  assert.ok(firstJournal.length < JSON.stringify(original).length / 10, "frame writes a small append record");
  assert.equal(await fs.readFile(snapshotPath, "utf8"), firstSnapshot, "append does not rewrite the snapshot");

  original.turns[0].items.push({ id: "frame-2", result: { content: "y" } });
  await store.persist(original);
  const journalBeforeCompaction = await fs.readFile(journalPath, "utf8");
  assert.equal(journalBeforeCompaction.trim().split("\n").length, 2);
  original.turns[0].items.push({ id: "frame-3", text: "snapshot state" });
  await store.persist(original);
  const compacted = JSON.parse(await fs.readFile(snapshotPath, "utf8"));
  assert.equal(compacted.format, "studio-claude-session-v1", "periodic compaction writes a versioned snapshot");
  assert.equal((await fs.stat(journalPath)).size, 0, "compaction truncates the journal");

  // Model a crash after snapshot rename and before journal truncation.
  await fs.writeFile(journalPath, journalBeforeCompaction);
  await fs.appendFile(journalPath, '{"sequence":4');
  const recovered = await createSessionStore(root).get(id);
  assert.deepEqual(recovered, original, "snapshot sequence skips already-compacted records and ignores a torn tail");
  const resumedStore = createSessionStore(root);
  const resumed = await resumedStore.get(id);
  resumed.turns[0].items.push({ id: "after-recovery", text: "append remains valid" });
  await resumedStore.persist(resumed);
  assert.deepEqual(await createSessionStore(root).readAll(id).then((value) => value.session), resumed);
});

await temporary(async (root) => {
  const old = makeSession();
  await fs.mkdir(path.join(root, "sessions"), { recursive: true });
  await fs.writeFile(path.join(root, "sessions", id + ".json"), JSON.stringify(old));
  const loaded = await createSessionStore(root).get(id);
  assert.deepEqual(loaded, old, "legacy plain JSON session files remain readable");
});

await temporary(async (root) => {
  const store = createSessionStore(root, { maxCacheBytes: 1 });
  const large = makeSession();
  large.turns[0].items.push({ id: "payload", text: "p".repeat(1024) });
  store.sessions.set(id, large);
  await store.persist(large);
  assert.equal(store.sessions.size, 0, "cache byte limit evicts an oversized idle entry");
  assert.equal(store.stats().retainedBytes, 0);
});

// Report append cost, CPU time, and resident memory for both review sizes.
await temporary(async (root) => {
  for (const scenario of [
    { name: "263KB", historyBytes: 256 * 1024, frames: 20 },
    { name: "5MB", historyBytes: 5 * 1024 * 1024, frames: 200 },
  ]) {
    global.gc?.();
    const rssBefore = process.memoryUsage().rss;
    const scenarioRoot = path.join(root, scenario.name);
    const store = createSessionStore(scenarioRoot, { compactRecords: 10000, compactBytes: 64 * 1024 * 1024 });
    let large = makeSession();
    large.turns[0].items.push({ id: "history", type: "toolResult", result: { content: "h".repeat(scenario.historyBytes) } });
    store.sessions.set(id, large);
    await store.persist(large);
    const initialSessionBytes = Buffer.byteLength(JSON.stringify(large));
    const journal = path.join(scenarioRoot, "sessions", id + ".jsonl");
    let appendedBytes = 0, legacyBytes = 0, frameCpuMs = 0, frameWallMs = 0;
    let peakRss = process.memoryUsage().rss;
    for (let frame = 0; frame < scenario.frames; frame++) {
      large.turns[0].items.push({ id: `frame-${frame}`, type: "agentMessage", text: `frame ${frame}` });
      const before = frame === 0 ? 0 : (await fs.stat(journal)).size;
      const cpuStart = process.cpuUsage();
      const started = performance.now();
      await store.persist(large);
      const cpuDelta = process.cpuUsage(cpuStart);
      frameCpuMs += (cpuDelta.user + cpuDelta.system) / 1000;
      frameWallMs += performance.now() - started;
      const after = (await fs.stat(journal)).size;
      appendedBytes += after - before;
      legacyBytes += Buffer.byteLength(JSON.stringify(large));
      peakRss = Math.max(peakRss, process.memoryUsage().rss);
    }
    const rssBeforeEviction = process.memoryUsage().rss;
    const retainedCacheBeforeEviction = store.stats().retainedBytes;
    await store.evict(id);
    large = null;
    global.gc?.();
    const rssAfterEviction = process.memoryUsage().rss;
    assert.equal(store.stats().retainedBytes, 0);
    console.log(JSON.stringify({
      syntheticHistoryBytes: scenario.historyBytes,
      initialSessionJsonBytes: initialSessionBytes,
      frames: scenario.frames,
      appendBytes: appendedBytes,
      appendBytesPerFrame: Math.round(appendedBytes / scenario.frames),
      legacyRewriteBytes: legacyBytes,
      cpuMsPerFrame: Number((frameCpuMs / scenario.frames).toFixed(3)),
      wallMsPerFrame: Number((frameWallMs / scenario.frames).toFixed(3)),
      rssBeforeSession: rssBefore,
      rssPeak: peakRss,
      rssBeforeEviction,
      rssAfterEviction,
      retainedCacheBytesBeforeEviction: retainedCacheBeforeEviction,
      retainedCacheBytesAfterEviction: store.stats().retainedBytes,
    }));
  }
});
