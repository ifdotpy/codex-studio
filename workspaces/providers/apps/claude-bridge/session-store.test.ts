import type { Session, Turn } from "./session-state.ts";
import assert from "node:assert/strict";
import { test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  createSessionStore,
  ephemeralSessionOptions,
} from "./session-store.ts";

function turnAt(session: Session, index: number): Turn {
  const turn = session.turns[index];
  assert.ok(turn, "Expected fixture turn");
  return turn;
}
const sessionId = "11111111-1111-4111-8111-111111111111";
const makeSession = (): Session => ({
  id: sessionId,
  cwd: "/tmp/work",
  turns: [
    {
      id: "turn-1",
      status: "completed",
      items: [{ id: "user-1", content: [{ type: "text", text: "hello" }] }],
    },
  ],
});
const withTemporaryRoot = async (run: (root: string) => Promise<void>) => {
  const root = await fs.mkdtemp(
    path.join(os.tmpdir(), "claude-session-store-"),
  );
  try {
    await run(root);
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
};

test("session history and metadata restore after cache eviction", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root);
    const original = makeSession();
    store.sessions.set(sessionId, original);
    await store.persist(original);
    turnAt(original, 0).items.push({ id: "answer-1", text: "saved" });
    await store.persist(original);
    await store.evict(sessionId);
    assert.equal(
      store.sessions.has(sessionId),
      false,
      "settled idle session leaves the cache",
    );
    const restored = await store.get(sessionId);
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
    const firstMetadata = await store.metadata(sessionId);
    assert.equal(firstMetadata.id, sessionId);
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
  }));

test("metadata revision changes only when persisted history changes", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root);
    const original = makeSession();
    store.sessions.set(sessionId, original);
    await store.persist(original);
    const before = await store.metadata(sessionId);
    turnAt(original, 0).items.push({ id: "changed", text: "history changed" });
    await store.persist(original);
    const after = await store.metadata(sessionId);
    assert.notEqual(
      after.revision,
      before.revision,
      "persist updates the stored history revision",
    );
    assert.equal(
      (await store.metadata(sessionId)).revision,
      after.revision,
      "metadata reads reuse the stored revision without serializing turns",
    );
  }));

test("held session identity survives concurrent load and persist", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root);
    const original = makeSession();
    store.sessions.set(sessionId, original);
    await store.persist(original);
    const heldByHandler = await store.get(sessionId);
    await store.evict(sessionId);
    const loading = store.get(sessionId);
    turnAt(heldByHandler, 0).items.push({
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
    turnAt(heldByHandler, 0).items.push({
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
      (await createSessionStore(root).readAll(sessionId)).session,
      heldByHandler,
      "persist does not journal a stale copy over concurrent state",
    );
    await assert.rejects(
      store.persist(structuredClone(heldByHandler)),
      /not the cached identity/,
      "a different object with the same session id cannot replace a live cached object",
    );
  }));

test("journal appends compact and recover torn records", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, {
      compactRecords: 3,
      compactBytes: 1024 * 1024,
    });
    const original = makeSession();
    turnAt(original, 0).items.push({
      id: "prior-history",
      result: { content: "h".repeat(4096) },
    });
    store.sessions.set(sessionId, original);
    await store.persist(original);
    const snapshotPath = path.join(root, "sessions", sessionId + ".json");
    const journalPath = path.join(root, "sessions", sessionId + ".jsonl");
    const firstSnapshot = await fs.readFile(snapshotPath, "utf8");
    turnAt(original, 0).items.push({ id: "frame-1", text: "small update" });
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

    turnAt(original, 0).items.push({ id: "frame-2", result: { content: "y" } });
    await store.persist(original);
    const journalBeforeCompaction = await fs.readFile(journalPath, "utf8");
    assert.equal(journalBeforeCompaction.trim().split("\n").length, 2);
    turnAt(original, 0).items.push({ id: "frame-3", text: "snapshot state" });
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
    const recovered = await createSessionStore(root).get(sessionId);
    assert.deepEqual(
      recovered,
      original,
      "snapshot sequence skips already-compacted records and ignores a torn tail",
    );
    const resumedStore = createSessionStore(root);
    const resumed = await resumedStore.get(sessionId);
    turnAt(resumed, 0).items.push({
      id: "after-recovery",
      text: "append remains valid",
    });
    await resumedStore.persist(resumed);
    assert.deepEqual(
      await createSessionStore(root)
        .readAll(sessionId)
        .then((value) => value.session),
      resumed,
    );
    await resumedStore.evict(sessionId);
    assert.equal(resumedStore.stats().retainedBytes, 0);
  }));

test("legacy plain JSON session files remain readable", async () =>
  withTemporaryRoot(async (root) => {
    const old = makeSession();
    await fs.mkdir(path.join(root, "sessions"), { recursive: true });
    await fs.writeFile(
      path.join(root, "sessions", sessionId + ".json"),
      JSON.stringify(old),
    );
    const loaded = await createSessionStore(root).get(sessionId);
    assert.deepEqual(
      loaded,
      old,
      "legacy plain JSON session files remain readable",
    );
  }));

test("cache byte limit evicts an oversized idle session", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { maxCacheBytes: 1 });
    const large = makeSession();
    turnAt(large, 0).items.push({ id: "payload", text: "p".repeat(1024) });
    store.sessions.set(sessionId, large);
    await store.persist(large);
    assert.equal(
      store.sessions.size,
      0,
      "cache byte limit evicts an oversized idle entry",
    );
    assert.equal(store.stats().retainedBytes, 0);
  }));

test("mutation during a save survives the following persist", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeSession();
    await store.persist(session);
    session.turns.push({ id: "turn-2", status: "inProgress", items: [] });
    const originalAppend = fs.appendFile;
    const { promise: held, resolve: entered } = Promise.withResolvers<void>();
    const { promise: gate, resolve: release } = Promise.withResolvers<void>();
    fs.appendFile = async (...args) => {
      const result = await originalAppend(...args);
      entered();
      await gate;
      return result;
    };
    try {
      const saved = store.persist(session);
      await held;
      turnAt(session, 1).error = { message: "interrupted" };
      release();
      await saved;
    } finally {
      fs.appendFile = originalAppend;
    }
    turnAt(session, 1).status = "failed";
    const pendingError = turnAt(session, 1).error;
    assert.ok(pendingError);
    pendingError.message = "not submitted";
    await store.persist(session);
    const restored = await createSessionStore(root).get(sessionId);
    assert.equal(turnAt(restored, 1).error?.message, "not submitted");
    assert.equal(turnAt(restored, 1).status, "failed");
  }));

test("ephemeral title session stays live without writing durable session files", async () => {
  const id = "22222222-2222-4222-8222-222222222222";
  await withTemporaryRoot(async (root) => {
    const store = createSessionStore(root);
    const ephemeral = {
      id,
      cwd: "/tmp/title",
      ephemeral: true,
      createdAt: 1,
      turns: [{ id: "title-turn", status: "completed", items: [] }],
    };
    store.sessions.set(id, ephemeral);
    await store.persist(ephemeral);
    assert.equal(await store.get(id), ephemeral);
    assert.equal((await store.metadata(id)).revision, `ephemeral:${id}`);
    // Listing waits for store initialization; ephemeral persistence does not.
    assert.deepEqual(await store.listMetadata(), {
      data: [],
      nextCursor: null,
    });
    assert.deepEqual(await fs.readdir(path.join(root, "sessions")), []);
    await store.evict(id);
    await assert.rejects(store.get(id), { code: "ENOENT" });
  });
});

test("ephemeral Claude title query matches Python's persistence and tool flags", () => {
  assert.deepEqual(ephemeralSessionOptions({ ephemeral: true }), {
    persistSession: false,
    permissionMode: "dontAsk",
    tools: [],
    allowedTools: [],
    settingSources: [],
    strictMcpConfig: true,
    mcpServers: {},
  });
  assert.deepEqual(ephemeralSessionOptions({ ephemeral: false }), {});
});

test("metadata write failure retries without a new journal change", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeSession();
    session.preview = "old preview";
    session.updatedAt = 1;
    await store.persist(session);
    const before = await store.metadata(sessionId);
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
    const saved = await createSessionStore(root).metadata(sessionId);
    assert.equal(saved.preview, "new preview");
    assert.notEqual(saved.revision, before.revision);
  }));

test("partial append failure preserves the next queued write", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeSession();
    await store.persist(session);
    const journal = path.join(root, "sessions", sessionId + ".jsonl");
    const originalAppend = fs.appendFile;
    let failFirstAppend = true;
    const { promise: appendEntered, resolve: enterAppend } =
      Promise.withResolvers<void>();
    const { promise: appendGate, resolve: releaseAppend } =
      Promise.withResolvers<void>();
    turnAt(session, 0).items.push({ id: "partial-write", text: "preserved" });
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
    turnAt(session, 0).items.push({ id: "queued-write", text: "also saved" });
    const queuedWrite = store.persist(session);
    releaseAppend();
    try {
      await assert.rejects(failedWrite, /injected partial append/);
      await queuedWrite;
    } finally {
      fs.appendFile = originalAppend;
    }
    assert.deepEqual(
      (await createSessionStore(root).readAll(sessionId)).session,
      session,
    );
  }));

test("failed append rollback repairs journal before a no-op persist", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeSession();
    await store.persist(session);
    const originalSession = structuredClone(session);
    const journal = path.join(root, "sessions", sessionId + ".jsonl");
    const originalAppend = fs.appendFile;
    const originalTruncate = fs.truncate;
    turnAt(session, 0).items.push({ id: "rolled-back", text: "remove me" });
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

    turnAt(session, 0).items.pop();
    assert.deepEqual(session, originalSession);
    await store.persist(session);
    assert.equal((await fs.stat(journal)).size, 0);
    assert.deepEqual(
      (await createSessionStore(root).readAll(sessionId)).session,
      originalSession,
    );
  }));

test("restart truncates a torn tail after a Unicode journal record", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeSession();
    await store.persist(session);
    turnAt(session, 0).items.push({
      id: "unicode-record",
      text: "Привет 🌍",
    });
    await store.persist(session);
    const journal = path.join(root, "sessions", sessionId + ".jsonl");
    const completeBytes = (await fs.stat(journal)).size;
    await fs.appendFile(journal, '{"sequence":2');

    const restarted = createSessionStore(root);
    const recovered = await restarted.get(sessionId);
    assert.deepEqual(recovered, session);
    assert.equal((await fs.stat(journal)).size, completeBytes);
    turnAt(recovered, 0).items.push({
      id: "after-restart",
      text: "still valid",
    });
    await restarted.persist(recovered);
    assert.deepEqual(
      (await createSessionStore(root).readAll(sessionId)).session,
      recovered,
    );
  }));

test("full append that reports failure rolls back before retry", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeSession();
    await store.persist(session);
    const journal = path.join(root, "sessions", sessionId + ".jsonl");
    const originalAppend = fs.appendFile;
    turnAt(session, 0).items.push({ id: "full-write", text: "retry me" });
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
      (await createSessionStore(root).readAll(sessionId)).session,
      session,
    );
  }));

test("snapshot publish failure keeps journal replayable", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1 });
    const session = makeSession();
    await store.persist(session);
    const snapshot = path.join(root, "sessions", sessionId + ".json");
    const originalRename = fs.rename;
    turnAt(session, 0).items.push({ id: "before-snapshot", text: "saved" });
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
      (await createSessionStore(root).readAll(sessionId)).session,
      session,
    );
  }));

test("reported journal clear error refreshes length before later append retry", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1 });
    const session = makeSession();
    await store.persist(session);
    const journal = path.join(root, "sessions", sessionId + ".jsonl");
    const originalWriteFile = fs.writeFile;
    turnAt(session, 0).items.push({ id: "compacted", text: "saved" });
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
    turnAt(session, 0).items.push({ id: "partial-after-clear", text: "retry" });
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
      if (to === path.join(root, "sessions", sessionId + ".json"))
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
      (await createSessionStore(root).readAll(sessionId)).session,
      session,
    );
  }));

test("saves queued behind a running save join into one later save", async () =>
  withTemporaryRoot(async (root) => {
    const store = createSessionStore(root, { compactRecords: 1000 });
    const session = makeSession();
    await store.persist(session);
    const journal = path.join(root, "sessions", sessionId + ".jsonl");
    const originalAppend = fs.appendFile;
    let appends = 0;
    const { promise: appendEntered, resolve: enterAppend } =
      Promise.withResolvers<void>();
    const { promise: appendGate, resolve: releaseAppend } =
      Promise.withResolvers<void>();
    fs.appendFile = async (file, data, ...options) => {
      if (file === journal && appends++ === 0) {
        enterAppend();
        await appendGate;
      }
      return originalAppend(file, data, ...options);
    };
    try {
      turnAt(session, 0).items.push({ id: "running", text: "first" });
      const running = store.persist(session);
      await appendEntered;
      const waiting = [];
      for (const index of [1, 2, 3]) {
        turnAt(session, 0).items.push({ id: "queued-" + index, text: "later" });
        waiting.push(store.persist(session));
      }
      assert.equal(waiting[0], waiting[1]);
      assert.equal(waiting[1], waiting[2]);
      releaseAppend();
      await running;
      await Promise.all(waiting);
    } finally {
      fs.appendFile = originalAppend;
    }
    assert.equal(appends, 2, "three waiting saves make one journal record");
    assert.deepEqual(
      (await createSessionStore(root).readAll(sessionId)).session,
      session,
    );
  }));
