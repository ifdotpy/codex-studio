import assert from "node:assert/strict";
import {
  copyLocalDraftScope,
  localDraftKey,
  readLocalDraftRecord,
  readLocalDrafts,
  writeLocalDraftRecord,
} from "./draftStorage.ts";
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
