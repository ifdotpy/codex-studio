import assert from "node:assert/strict";
import {
  copyLocalDraftScope,
  localDraftKey,
  readLocalDraftRecord,
  readLocalDrafts,
  writeLocalDraftRecord,
} from "./draftStorage.ts";
import { draftVersionId } from "./draftIdentity.ts";
import { activeDraftVersions } from "./draftVersions.ts";
import { draftConflictHandler } from "./conflicts.ts";
import { test } from "vitest";

const originalLocalStorage = Object.getOwnPropertyDescriptor(
  globalThis,
  "localStorage",
);

test("current draft records stay scoped and never inspect the retired map", () => {
  try {
    class StorageFixture {
      values = new Map();
      get length() {
        return this.values.size;
      }
      key(index) {
        return [...this.values.keys()][index] ?? null;
      }
      getItem(key) {
        assert.doesNotMatch(key, /^codex-drafts:[^:]+$/);
        return this.values.get(key) ?? null;
      }
      setItem(key, value) {
        assert.doesNotMatch(key, /^codex-drafts:[^:]+$/);
        this.values.set(String(key), String(value));
      }
      removeItem(key) {
        assert.doesNotMatch(key, /^codex-drafts:[^:]+$/);
        this.values.delete(key);
      }
    }

    const storage = new StorageFixture();
    globalThis.localStorage = storage;
    const a = `codex-drafts:${"a".repeat(32)}`;
    const b = `codex-drafts:${"b".repeat(32)}`;
    const retiredMap = JSON.stringify({ lead: "Retired draft", other: "Old" });
    storage.values.set(a, retiredMap);

    writeLocalDraftRecord(a, "lead", "Current local draft", "local", {
      updated: 10,
    });
    writeLocalDraftRecord(a, "removed", null);
    assert.deepEqual(readLocalDrafts(a), { lead: "Current local draft" });
    assert.equal(readLocalDraftRecord(a, "lead").updated, 10);
    assert.equal(readLocalDraftRecord(a, "removed").deleted, true);

    copyLocalDraftScope(a, b);
    assert.deepEqual(readLocalDrafts(b), { lead: "Current local draft" });
    assert.equal(readLocalDraftRecord(b, "lead").workspace, b.slice(13));
    assert.equal(storage.values.get(a), retiredMap);
    assert.equal(storage.values.has(b), false);
    assert.deepEqual(JSON.parse(storage.values.get(localDraftKey(b, "lead"))), {
      ...JSON.parse(storage.values.get(localDraftKey(a, "lead"))),
      workspace: b.slice("codex-drafts:".length),
    });
  } finally {
    if (originalLocalStorage)
      Object.defineProperty(globalThis, "localStorage", originalLocalStorage);
    else delete globalThis.localStorage;
  }
});

test("reads and rewrites current-key records left by the old migration", () => {
  const originalLocalStorage = Object.getOwnPropertyDescriptor(
    globalThis,
    "localStorage",
  );
  try {
    const values = new Map();
    globalThis.localStorage = {
      get length() {
        return values.size;
      },
      key(index) {
        return [...values.keys()][index] ?? null;
      },
      getItem(key) {
        return values.get(key) ?? null;
      },
      setItem(key, value) {
        values.set(key, String(value));
      },
    };
    const scope = `codex-drafts:${"c".repeat(32)}`;
    const migrated = (session, text) =>
      JSON.stringify({
        version: 1,
        workspace: scope.slice("codex-drafts:".length),
        session,
        text,
        deleted: false,
        source: "legacy",
        legacyBaselineHash: "old-hash",
        future: 1,
      });
    values.set(localDraftKey(scope, "read"), migrated("read", "visible"));
    values.set(localDraftKey(scope, "local"), migrated("local", "before"));
    values.set(localDraftKey(scope, "remote"), migrated("remote", "before"));
    values.set(localDraftKey(scope, "copy"), migrated("copy", "copied"));

    assert.deepEqual(readLocalDrafts(scope), {
      read: "visible",
      local: "before",
      remote: "before",
      copy: "copied",
    });
    assert.equal(readLocalDraftRecord(scope, "read").source, "local");
    writeLocalDraftRecord(scope, "local", "local write", "local");
    writeLocalDraftRecord(scope, "remote", "remote write", "remote");
    const local = JSON.parse(values.get(localDraftKey(scope, "local")));
    const remote = JSON.parse(values.get(localDraftKey(scope, "remote")));
    assert.equal(local.source, "local");
    assert.equal(remote.source, "remote");
    for (const record of [local, remote]) {
      assert.equal(
        record.future,
        1,
        "unknown current-format fields are retained",
      );
      assert.equal("legacyBaselineHash" in record, false);
    }
    const destination = `codex-drafts:${"d".repeat(32)}`;
    copyLocalDraftScope(scope, destination);
    const copied = JSON.parse(values.get(localDraftKey(destination, "copy")));
    assert.equal(copied.text, "copied");
    assert.equal(copied.source, "local");
    assert.equal("legacyBaselineHash" in copied, false);
  } finally {
    if (originalLocalStorage)
      Object.defineProperty(globalThis, "localStorage", originalLocalStorage);
    else delete globalThis.localStorage;
  }
});

test("corruption stays scoped and draft versions retain conflict alternatives", async () => {
  const originalLocalStorage = Object.getOwnPropertyDescriptor(
    globalThis,
    "localStorage",
  );
  try {
    const values = new Map();
    globalThis.localStorage = {
      get length() {
        return values.size;
      },
      key(index) {
        return [...values.keys()][index] ?? null;
      },
      getItem(key) {
        return values.get(key) ?? null;
      },
      setItem(key, value) {
        values.set(key, String(value));
      },
    };
    const scope = `codex-drafts:${"a".repeat(32)}`;
    values.set(localDraftKey(scope, "broken"), "{");
    values.set(`codex-chat-draft:${"a".repeat(32)}:%E0%A4%A`, "{}");
    writeLocalDraftRecord(scope, "healthy", "keep visible");
    let corruptCount = 0;
    assert.deepEqual(
      readLocalDrafts(scope, () => corruptCount++),
      {
        healthy: "keep visible",
      },
    );
    assert.equal(corruptCount, 2);

    const tabA = draftVersionId("device", "tab-a", "chat");
    const tabB = draftVersionId("device", "tab-b", "chat");
    assert.notEqual(tabA, tabB);
    const original = {
      id: "device:old:chat",
      session: "chat",
      device: "device",
      text: "Old text",
      updated: 10,
    };
    const replacement = {
      id: tabB,
      session: "chat",
      device: "device",
      text: "New text",
      updated: 11,
      seen: { [original.id]: 10 },
    };
    assert.deepEqual(
      activeDraftVersions([original, replacement]).map((row) => row.id),
      [tabB],
    );
    assert.deepEqual(
      activeDraftVersions([original, { ...replacement, seen: {} }]).map(
        (row) => row.id,
      ),
      [tabB, original.id],
    );

    const merge = async (master, fork) =>
      JSON.parse(
        (
          await draftConflictHandler.resolve({
            realMasterState: {
              id: master.id,
              payload: JSON.stringify(master),
              seq: 0,
            },
            newDocumentState: {
              id: fork.id,
              payload: JSON.stringify(fork),
              seq: 0,
            },
          })
        ).payload,
      );
    const alternatives = await merge(
      {
        id: tabA,
        text: "Master text",
        updated: 10,
        seen: {},
        alternatives: ["Earlier"],
      },
      {
        id: tabB,
        text: "Fork text",
        updated: 11,
        seen: {},
        alternatives: ["Other"],
      },
    );
    assert.equal(alternatives.text, "Fork text");
    assert.deepEqual(alternatives.alternatives, [
      "Earlier",
      "Master text",
      "Other",
    ]);
    const retired = await merge(
      { id: original.id, text: "Old text", updated: 10, seen: {} },
      { ...replacement, seen: { [original.id]: 10 } },
    );
    assert.deepEqual(retired.alternatives, []);
  } finally {
    if (originalLocalStorage)
      Object.defineProperty(globalThis, "localStorage", originalLocalStorage);
    else delete globalThis.localStorage;
  }
});

test("quota failure preserves the current draft record and pending journal", () => {
  const originalLocalStorage = Object.getOwnPropertyDescriptor(
    globalThis,
    "localStorage",
  );
  try {
    class QuotaStorage {
      values = new Map();
      capacity = Infinity;
      get length() {
        return this.values.size;
      }
      key(index) {
        return [...this.values.keys()][index] ?? null;
      }
      getItem(key) {
        return this.values.get(key) ?? null;
      }
      setItem(key, value) {
        const next =
          [...this.values].reduce(
            (total, [name, text]) =>
              total + (name === key ? 0 : (name.length + text.length) * 2),
            0,
          ) +
          (String(key).length + String(value).length) * 2;
        if (next > this.capacity) throw new Error("QuotaExceededError");
        this.values.set(String(key), String(value));
      }
    }
    const storage = new QuotaStorage();
    globalThis.localStorage = storage;
    const scope = `codex-drafts:${"b".repeat(32)}`;
    writeLocalDraftRecord(scope, "chat", "saved text");
    const journalKey = `${scope}:pending:tab:chat`;
    storage.setItem(journalKey, JSON.stringify({ text: "journal text" }));
    const record = storage.getItem(localDraftKey(scope, "chat"));
    const journal = storage.getItem(journalKey);
    const usedBytes = [...storage.values].reduce(
      (total, [key, value]) => total + (key.length + value.length) * 2,
      0,
    );
    storage.capacity = usedBytes + 10;
    assert.throws(
      () => writeLocalDraftRecord(scope, "chat", "x".repeat(1000)),
      /QuotaExceededError/,
    );
    assert.equal(storage.getItem(localDraftKey(scope, "chat")), record);
    assert.equal(storage.getItem(journalKey), journal);
    assert.equal(readLocalDraftRecord(scope, "chat").text, "saved text");
  } finally {
    if (originalLocalStorage)
      Object.defineProperty(globalThis, "localStorage", originalLocalStorage);
    else delete globalThis.localStorage;
  }
});
