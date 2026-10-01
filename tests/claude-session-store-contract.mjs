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

// Report reproducible storage and retained-cache costs for a large synthetic history.
await temporary(async (root) => {
  const store = createSessionStore(root, { compactRecords: 10000, compactBytes: 64 * 1024 * 1024 });
  const large = makeSession();
  large.turns[0].items.push({ id: "history", type: "toolResult", result: { content: "h".repeat(256 * 1024) } });
  store.sessions.set(id, large);
  await store.persist(large);
  let appended = 0;
  for (let frame = 0; frame < 20; frame++) {
    large.turns[0].items.push({ id: `frame-${frame}`, type: "agentMessage", text: `frame ${frame}` });
    const journal = path.join(root, "sessions", id + ".jsonl");
    const before = frame === 0 ? 0 : (await fs.stat(journal)).size;
    await store.persist(large);
    const after = (await fs.stat(journal)).size;
    appended += after - before;
  }
  const legacyBytesPerFrame = Buffer.byteLength(JSON.stringify(large));
  const beforeEviction = store.stats().retainedBytes;
  await store.evict(id);
  const afterEviction = store.stats().retainedBytes;
  console.log(JSON.stringify({
    syntheticHistoryBytes: Buffer.byteLength(JSON.stringify(large)),
    frames: 20,
    appendBytes: appended,
    appendBytesPerFrame: Math.round(appended / 20),
    legacyBytesPerFrame,
    legacyBytesForFrames: legacyBytesPerFrame * 20,
    retainedCacheBytesBeforeEviction: beforeEviction,
    retainedCacheBytesAfterEviction: afterEviction,
  }));
  assert.equal(afterEviction, 0);
});
