import assert from "node:assert/strict";
import { test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createSessionStore } from "./session-store.mjs";

const faultSessionId = "11111111-1111-4111-8111-111111111111";
const makeFaultSession = () => ({
  id: faultSessionId,
  cwd: "/tmp/work",
  turns: [
    {
      id: "turn-1",
      status: "completed",
      items: [{ id: "user-1", content: [{ type: "text", text: "hello" }] }],
    },
  ],
});
const withTemporaryRoot = async (run) => {
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "claude-session-store-"),
  );
  try {
    await run(root);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
};

test("session store persists, restores and coordinates cached writes", async () => {
  const id = "11111111-1111-4111-8111-111111111111";
  const makeSession = () => ({
    id,
    cwd: "/tmp/work",
    turns: [
      {
        id: "turn-1",
        status: "completed",
        items: [{ id: "user-1", content: [{ type: "text", text: "hello" }] }],
      },
    ],
  });
  const temporary = async (run) => {
    const root = await fs.mkdtemp(
      path.join(os.tmpdir(), "claude-session-store-"),
    );
    try {
      await run(root);
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  };

  await temporary(async (root) => {
    const store = createSessionStore(root);
    const original = makeSession();
    store.sessions.set(id, original);
    await store.persist(original);
    original.turns[0].items.push({ id: "answer-1", text: "saved" });
    await store.persist(original);
    await store.evict(id);
    assert.equal(
      store.sessions.has(id),
      false,
      "settled idle session leaves the cache",
    );
    const restored = await store.get(id);
    assert.deepEqual(
      restored,
      original,
      "session history and request identity reload exactly",
    );
    assert.equal(
      store.stats().pendingWrites,
      0,
      "settled write promise is removed",
    );
    const firstMetadata = await store.metadata(id);
    assert.equal(firstMetadata.id, id);
    assert.equal(typeof firstMetadata.revision, "string");
    assert.equal(
      "turns" in firstMetadata,
      false,
      "sidecar metadata excludes transcript bodies",
    );
    assert.deepEqual(await store.listMetadata(null, 1), {
      data: [firstMetadata],
      nextCursor: null,
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
    assert.notEqual(
      after.revision,
      before.revision,
      "persist updates the stored history revision",
    );
    assert.equal(
      (await store.metadata(id)).revision,
      after.revision,
      "metadata reads reuse the stored revision without serializing turns",
    );
  });

  await temporary(async (root) => {
    const store = createSessionStore(root);
    const original = makeSession();
    store.sessions.set(id, original);
    await store.persist(original);
    const heldByHandler = await store.get(id);
    await store.evict(id);
    const loading = store.get(id);
    heldByHandler.turns[0].items.push({
      id: "during-load",
      text: "written by the held object",
    });
    const saving = store.persist(heldByHandler);
    const reloaded = await loading;
    await saving;
    assert.strictEqual(
      reloaded,
      heldByHandler,
      "weak reference reuses an object held across an await",
    );
    heldByHandler.turns[0].items.push({
      id: "after-race",
      text: "current state",
    });
    await store.persist(heldByHandler);
    assert.equal(
      store.stats().retainedBytes,
      Buffer.byteLength(JSON.stringify(heldByHandler)) * 2,
      "incremental cache bytes track the live session and shadow",
    );
    assert.deepEqual(
      (await createSessionStore(root).readAll(id)).session,
      heldByHandler,
      "persist does not journal a stale copy over concurrent state",
    );
    await assert.rejects(
      store.persist(structuredClone(heldByHandler)),
      /not the cached identity/,
      "a different object with the same session id cannot replace a live cached object",
    );
  });

  await temporary(async (root) => {
    const store = createSessionStore(root, {
      compactRecords: 3,
      compactBytes: 1024 * 1024,
    });
    const original = makeSession();
    original.turns[0].items.push({
      id: "prior-history",
      result: { content: "h".repeat(4096) },
    });
    store.sessions.set(id, original);
    await store.persist(original);
    const snapshotPath = path.join(root, "sessions", id + ".json");
    const journalPath = path.join(root, "sessions", id + ".jsonl");
    const firstSnapshot = await fs.readFile(snapshotPath, "utf8");
    original.turns[0].items.push({ id: "frame-1", text: "small update" });
    await store.persist(original);
    const firstJournal = await fs.readFile(journalPath, "utf8");
    assert.ok(
      firstJournal.length < JSON.stringify(original).length / 10,
      "frame writes a small append record",
    );
    assert.equal(
      await fs.readFile(snapshotPath, "utf8"),
      firstSnapshot,
      "append does not rewrite the snapshot",
    );

    original.turns[0].items.push({ id: "frame-2", result: { content: "y" } });
    await store.persist(original);
    const journalBeforeCompaction = await fs.readFile(journalPath, "utf8");
    assert.equal(journalBeforeCompaction.trim().split("\n").length, 2);
    original.turns[0].items.push({ id: "frame-3", text: "snapshot state" });
    await store.persist(original);
    const compacted = JSON.parse(await fs.readFile(snapshotPath, "utf8"));
    assert.equal(
      compacted.format,
      "studio-claude-session-v1",
      "periodic compaction writes a versioned snapshot",
    );
    assert.equal(
      (await fs.stat(journalPath)).size,
      0,
      "compaction truncates the journal",
    );

    // Model a crash after snapshot rename and before journal truncation.
    await fs.writeFile(journalPath, journalBeforeCompaction);
    await fs.appendFile(journalPath, '{"sequence":4');
    const recovered = await createSessionStore(root).get(id);
    assert.deepEqual(
      recovered,
      original,
      "snapshot sequence skips already-compacted records and ignores a torn tail",
    );
    const resumedStore = createSessionStore(root);
    const resumed = await resumedStore.get(id);
    resumed.turns[0].items.push({
      id: "after-recovery",
      text: "append remains valid",
    });
    await resumedStore.persist(resumed);
    assert.deepEqual(
      await createSessionStore(root)
        .readAll(id)
        .then((value) => value.session),
      resumed,
    );
  });

  await temporary(async (root) => {
    const old = makeSession();
    await fs.mkdir(path.join(root, "sessions"), { recursive: true });
    await fs.writeFile(
      path.join(root, "sessions", id + ".json"),
      JSON.stringify(old),
    );
    const loaded = await createSessionStore(root).get(id);
    assert.deepEqual(
      loaded,
      old,
      "legacy plain JSON session files remain readable",
    );
  });

  await temporary(async (root) => {
    const store = createSessionStore(root, { maxCacheBytes: 1 });
    const large = makeSession();
    large.turns[0].items.push({ id: "payload", text: "p".repeat(1024) });
    store.sessions.set(id, large);
    await store.persist(large);
    assert.equal(
      store.sessions.size,
      0,
      "cache byte limit evicts an oversized idle entry",
    );
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
      const store = createSessionStore(scenarioRoot, {
        compactRecords: 10000,
        compactBytes: 64 * 1024 * 1024,
      });
      let large = makeSession();
      large.turns[0].items.push({
        id: "history",
        type: "toolResult",
        result: { content: "h".repeat(scenario.historyBytes) },
      });
      store.sessions.set(id, large);
      await store.persist(large);
      const initialSessionBytes = Buffer.byteLength(JSON.stringify(large));
      const journal = path.join(scenarioRoot, "sessions", id + ".jsonl");
      let appendedBytes = 0,
        legacyBytes = 0,
        frameCpuMs = 0,
        frameWallMs = 0;
      let peakRss = process.memoryUsage().rss;
      for (let frame = 0; frame < scenario.frames; frame++) {
        large.turns[0].items.push({
          id: `frame-${frame}`,
          type: "agentMessage",
          text: `frame ${frame}`,
        });
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
      console.log(
        JSON.stringify({
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
        }),
      );
    }
  });

  await temporary(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeSession();
    await store.persist(session);
    session.turns.push({ id: "turn-2", status: "inProgress", items: [] });
    const originalAppend = fs.appendFile;
    let entered, release;
    const held = new Promise((resolve) => {
      entered = resolve;
    });
    const gate = new Promise((resolve) => {
      release = resolve;
    });
    fs.appendFile = async (...args) => {
      const result = await originalAppend(...args);
      entered();
      await gate;
      return result;
    };
    try {
      const saved = store.persist(session);
      await held;
      session.turns[1].error = { message: "interrupted" };
      release();
      await saved;
    } finally {
      fs.appendFile = originalAppend;
    }
    session.turns[1].status = "failed";
    session.turns[1].error.message = "not submitted";
    await store.persist(session);
    const restored = await createSessionStore(root).get(id);
    assert.equal(restored.turns[1].error.message, "not submitted");
    assert.equal(restored.turns[1].status, "failed");
  });
});

test("metadata write failure retries without a new journal change", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeFaultSession();
    session.preview = "old preview";
    session.updatedAt = 1;
    await store.persist(session);
    const before = await store.metadata(faultSessionId);
    session.preview = "new preview";
    session.updatedAt = 2;

    const originalRename = fs.rename;
    fs.rename = async (from, to) => {
      if (String(to).endsWith(".meta.json"))
        throw Object.assign(new Error("injected metadata rename failure"), {
          code: "EIO",
        });
      return originalRename(from, to);
    };
    try {
      await assert.rejects(store.persist(session), /injected metadata rename/);
    } finally {
      fs.rename = originalRename;
    }

    await store.persist(session);
    const saved = await createSessionStore(root).metadata(faultSessionId);
    assert.equal(saved.preview, "new preview");
    assert.notEqual(saved.revision, before.revision);
  }));

test("partial append failure preserves the next queued write", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeFaultSession();
    await store.persist(session);
    const journal = path.join(root, "sessions", faultSessionId + ".jsonl");
    const originalAppend = fs.appendFile;
    let failFirstAppend = true;
    let enterAppend, releaseAppend;
    const appendEntered = new Promise((resolve) => {
      enterAppend = resolve;
    });
    const appendGate = new Promise((resolve) => {
      releaseAppend = resolve;
    });
    session.turns[0].items.push({ id: "partial-write", text: "preserved" });
    fs.appendFile = async (file, data, ...options) => {
      if (file === journal && failFirstAppend) {
        failFirstAppend = false;
        await originalAppend(file, String(data).slice(0, 16), ...options);
        enterAppend();
        await appendGate;
        throw Object.assign(new Error("injected partial append failure"), {
          code: "ENOSPC",
        });
      }
      return originalAppend(file, data, ...options);
    };
    const failedWrite = store.persist(session);
    await appendEntered;
    session.turns[0].items.push({ id: "queued-write", text: "also saved" });
    const queuedWrite = store.persist(session);
    releaseAppend();
    try {
      await assert.rejects(failedWrite, /injected partial append/);
      await queuedWrite;
    } finally {
      fs.appendFile = originalAppend;
    }
    assert.deepEqual(
      (await createSessionStore(root).readAll(faultSessionId)).session,
      session,
    );
  }));

test("failed append rollback repairs journal before a no-op persist", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeFaultSession();
    await store.persist(session);
    const originalSession = structuredClone(session);
    const journal = path.join(root, "sessions", faultSessionId + ".jsonl");
    const originalAppend = fs.appendFile;
    const originalTruncate = fs.truncate;
    session.turns[0].items.push({ id: "rolled-back", text: "remove me" });
    fs.appendFile = async (file, data, ...options) => {
      if (file === journal) {
        await originalAppend(file, String(data).slice(0, 16), ...options);
        throw Object.assign(new Error("injected append failure"), {
          code: "ENOSPC",
        });
      }
      return originalAppend(file, data, ...options);
    };
    fs.truncate = async (file, ...options) => {
      if (file === journal)
        throw Object.assign(new Error("injected rollback failure"), {
          code: "EIO",
        });
      return originalTruncate(file, ...options);
    };
    try {
      await assert.rejects(store.persist(session), AggregateError);
    } finally {
      fs.appendFile = originalAppend;
      fs.truncate = originalTruncate;
    }

    session.turns[0].items.pop();
    assert.deepEqual(session, originalSession);
    await store.persist(session);
    assert.equal((await fs.stat(journal)).size, 0);
    assert.deepEqual(
      (await createSessionStore(root).readAll(faultSessionId)).session,
      originalSession,
    );
  }));

test("restart truncates a torn tail after a Unicode journal record", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeFaultSession();
    await store.persist(session);
    session.turns[0].items.push({
      id: "unicode-record",
      text: "Привет 🌍",
    });
    await store.persist(session);
    const journal = path.join(root, "sessions", faultSessionId + ".jsonl");
    const completeBytes = (await fs.stat(journal)).size;
    await fs.appendFile(journal, '{"sequence":2');

    const restarted = createSessionStore(root);
    const recovered = await restarted.get(faultSessionId);
    assert.deepEqual(recovered, session);
    assert.equal((await fs.stat(journal)).size, completeBytes);
    recovered.turns[0].items.push({ id: "after-restart", text: "still valid" });
    await restarted.persist(recovered);
    assert.deepEqual(
      (await createSessionStore(root).readAll(faultSessionId)).session,
      recovered,
    );
  }));

test("full append that reports failure rolls back before retry", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeFaultSession();
    await store.persist(session);
    const journal = path.join(root, "sessions", faultSessionId + ".jsonl");
    const originalAppend = fs.appendFile;
    session.turns[0].items.push({ id: "full-write", text: "retry me" });
    fs.appendFile = async (file, data, ...options) => {
      if (file === journal) {
        await originalAppend(file, data, ...options);
        throw Object.assign(new Error("injected post-write failure"), {
          code: "EIO",
        });
      }
      return originalAppend(file, data, ...options);
    };
    try {
      await assert.rejects(store.persist(session), /injected post-write/);
    } finally {
      fs.appendFile = originalAppend;
    }
    assert.equal((await fs.stat(journal)).size, 0);
    await store.persist(session);
    assert.deepEqual(
      (await createSessionStore(root).readAll(faultSessionId)).session,
      session,
    );
  }));

test("snapshot publish failure keeps journal replayable", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1 });
    const session = makeFaultSession();
    await store.persist(session);
    const snapshot = path.join(root, "sessions", faultSessionId + ".json");
    const originalRename = fs.rename;
    session.turns[0].items.push({ id: "before-snapshot", text: "saved" });
    fs.rename = async (from, to) => {
      if (to === snapshot)
        throw Object.assign(new Error("injected snapshot publish failure"), {
          code: "EIO",
        });
      return originalRename(from, to);
    };
    try {
      await assert.rejects(store.persist(session), /injected snapshot publish/);
    } finally {
      fs.rename = originalRename;
    }
    assert.deepEqual(
      (await createSessionStore(root).readAll(faultSessionId)).session,
      session,
    );
  }));

test("reported journal clear error refreshes length before later append retry", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1 });
    const session = makeFaultSession();
    await store.persist(session);
    const journal = path.join(root, "sessions", faultSessionId + ".jsonl");
    const originalWriteFile = fs.writeFile;
    session.turns[0].items.push({ id: "compacted", text: "saved" });
    fs.writeFile = async (file, data, ...options) => {
      if (file === journal && data === "") {
        await originalWriteFile(file, data, ...options);
        throw Object.assign(new Error("injected post-clear failure"), {
          code: "EIO",
        });
      }
      return originalWriteFile(file, data, ...options);
    };
    try {
      await assert.rejects(store.persist(session), /injected post-clear/);
    } finally {
      fs.writeFile = originalWriteFile;
    }
    assert.equal((await fs.stat(journal)).size, 0);

    const originalAppend = fs.appendFile;
    session.turns[0].items.push({ id: "partial-after-clear", text: "retry" });
    fs.appendFile = async (file, data, ...options) => {
      if (file === journal) {
        await originalAppend(file, String(data).slice(0, 16), ...options);
        throw Object.assign(new Error("injected later append failure"), {
          code: "ENOSPC",
        });
      }
      return originalAppend(file, data, ...options);
    };
    try {
      await assert.rejects(store.persist(session), /injected later append/);
    } finally {
      fs.appendFile = originalAppend;
    }
    assert.equal((await fs.stat(journal)).size, 0);
    const originalRename = fs.rename;
    fs.rename = async (from, to) => {
      if (to === path.join(root, "sessions", faultSessionId + ".json"))
        throw Object.assign(new Error("injected snapshot publish failure"), {
          code: "EIO",
        });
      return originalRename(from, to);
    };
    try {
      await assert.rejects(
        store.persist(session),
        /injected snapshot publish failure/,
      );
    } finally {
      fs.rename = originalRename;
    }
    assert.deepEqual(
      (await createSessionStore(root).readAll(faultSessionId)).session,
      session,
    );
  }));
