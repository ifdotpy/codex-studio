// Exercise idempotent per-chat migration and scope isolation with localStorage.
import assert from "node:assert/strict";
import {
  migrateLegacyDrafts,
  legacyBaselineHash,
  readDraftsWithLegacyFallback,
  readLocalDraftRecord,
  readLocalDrafts,
  writeLocalDraftRecord,
  copyLocalDraftScope,
  localDraftKey,
} from "../web/src/sync/draftStorage.ts";
import { draftVersionId } from "../web/src/sync/draftIdentity.ts";
import { activeDraftVersions } from "../web/src/sync/draftVersions.ts";
import { draftConflictHandler } from "../web/src/sync/conflicts.ts";
import { encodeDraftPayload } from "../web/src/sync/draftPayload.ts";

class StorageFixture {
  values = new Map();
  failAfter = null;
  capacityBytes = Infinity;
  get usedBytes() {
    return [...this.values].reduce(
      (bytes, [key, value]) => bytes + (key.length + value.length) * 2,
      0,
    );
  }
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
    const keyText = String(key);
    const valueText = String(value);
    if (key.startsWith("codex-chat-draft:") && this.failAfter !== null) {
      if (this.failAfter === 0) {
        this.failAfter = null;
        throw new Error("Injected migration interruption");
      }
      this.failAfter--;
    }
    const old = this.values.get(keyText);
    const nextBytes =
      this.usedBytes -
      (old === undefined ? 0 : (keyText.length + old.length) * 2) +
      (keyText.length + valueText.length) * 2;
    if (nextBytes > this.capacityBytes) throw new Error("QuotaExceededError");
    this.values.set(keyText, valueText);
  }
  removeItem(key) {
    this.values.delete(key);
  }
}

globalThis.localStorage = new StorageFixture();
const storage = globalThis.localStorage;
const a = "codex-drafts:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const b = "codex-drafts:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const legacy = { lead: "Legacy lead", other: "Legacy other", empty: "" };
storage.setItem(a, JSON.stringify(legacy));
const originalBytes = storage.getItem(a);

storage.failAfter = 1;
assert.throws(() => migrateLegacyDrafts(a), /Injected migration interruption/);
assert.deepEqual(readDraftsWithLegacyFallback(a), legacy);
assert.equal(
  storage.getItem(a),
  originalBytes,
  "Migration leaves the legacy map intact",
);
assert.equal(readLocalDraftRecord(a, "lead").source, "legacy");
assert.equal(readLocalDraftRecord(a, "other"), null);

const retry = migrateLegacyDrafts(a);
assert.deepEqual(retry, {});
const beforeRepeat = [...storage.values.entries()];
assert.deepEqual(migrateLegacyDrafts(a), {});
assert.deepEqual(
  [...storage.values.entries()],
  beforeRepeat,
  "A repeat migration makes no writes",
);
assert.deepEqual(readLocalDrafts(a), legacy);

writeLocalDraftRecord(a, "lead", "New local edit");
storage.setItem(a, JSON.stringify(legacy));
assert.deepEqual(migrateLegacyDrafts(a), {});
assert.equal(
  readLocalDrafts(a).lead,
  "New local edit",
  "Legacy edits cannot overwrite a local edit",
);

writeLocalDraftRecord(a, "other", null);
storage.setItem(
  a,
  JSON.stringify({
    lead: "Legacy lead",
    other: "Fresh older-tab edit",
    empty: "",
  }),
);
assert.deepEqual(migrateLegacyDrafts(a), { other: "Fresh older-tab edit" });
assert.equal(
  readDraftsWithLegacyFallback(a).other,
  undefined,
  "The changed old-tab value is available for a separate conflict branch",
);
assert.equal(readLocalDraftRecord(a, "other").deleted, true);
writeLocalDraftRecord(a, "other", null, "local", {
  legacyBaseline: "Fresh older-tab edit",
});
assert.deepEqual(migrateLegacyDrafts(a), {});

copyLocalDraftScope(a, b);
assert.equal(readLocalDrafts(b).lead, "New local edit");
assert.equal(readLocalDrafts(b).other, undefined);
assert.equal(
  readLocalDraftRecord(b, "lead").workspace,
  b.slice("codex-drafts:".length),
);
assert.equal(
  readLocalDraftRecord(a, "lead").workspace,
  a.slice("codex-drafts:".length),
);
storage.setItem(
  a,
  JSON.stringify({
    lead: "Legacy lead",
    other: "Fresh older-tab edit",
    empty: "Changed by old tab",
  }),
);
assert.deepEqual(migrateLegacyDrafts(a), { empty: "Changed by old tab" });
assert.equal(readLocalDrafts(a).empty, "");
writeLocalDraftRecord(a, "empty", "Changed by old tab", "local", {
  legacyBaseline: "Changed by old tab",
});
storage.setItem(
  a,
  JSON.stringify({ lead: "Legacy lead", other: "Fresh older-tab edit" }),
);
assert.deepEqual(migrateLegacyDrafts(a), { empty: null });
writeLocalDraftRecord(a, "empty", null, "local", { legacyBaseline: null });
assert.equal(
  readLocalDrafts(a).empty,
  undefined,
  "Legacy tab removal becomes a durable clear",
);

let corrupt = false;
storage.setItem(localDraftKey(a, "other"), "{broken");
storage.setItem(
  localDraftKey(a, "valid"),
  JSON.stringify({
    version: 1,
    workspace: a.slice("codex-drafts:".length),
    session: "valid",
    text: "Unaffected record",
    deleted: false,
    source: "local",
  }),
);
storage.setItem(
  `codex-chat-draft:${a.slice("codex-drafts:".length)}:%E0%A4%A`,
  "{}",
);
storage.setItem(
  a,
  JSON.stringify({ other: "Legacy fallback", lead: "Stale legacy edit" }),
);
const recovered = readDraftsWithLegacyFallback(a, () => (corrupt = true));
assert.equal(corrupt, true, "Corrupt records are reported");
assert.equal(
  recovered.other,
  "Legacy fallback",
  "The corrupt record falls back to legacy text",
);
assert.equal(
  recovered.valid,
  "Unaffected record",
  "One corrupt record does not hide other chats",
);
let malformedReported = false;
migrateLegacyDrafts(a, () => (malformedReported = true));
assert.equal(
  malformedReported,
  true,
  "Malformed encoded keys report locally without aborting migration",
);
const tabA = draftVersionId("device", "tab-a", "lead");
const tabB = draftVersionId("device", "tab-b", "lead");
assert.notEqual(
  tabA,
  tabB,
  "Writers in separate tabs keep distinct version ids",
);
const legacyId = "device:lead";
const reloadedWriterId = draftVersionId("device", "new-tab", "lead");
const legacyBranch = {
  id: legacyId,
  session: "lead",
  device: "device",
  text: "Existing device draft",
  updated: 20,
};
const reloadedBranch = {
  id: reloadedWriterId,
  session: "lead",
  device: "device",
  text: "Edited after reload",
  updated: 21,
  seen: { [legacyId]: 20 },
};
assert.deepEqual(
  activeDraftVersions([legacyBranch, reloadedBranch]).map((v) => v.id),
  [reloadedWriterId],
  "A reloaded existing device recognizes and retires its legacy version without a phantom conflict",
);
assert.deepEqual(
  activeDraftVersions([legacyBranch, { ...reloadedBranch, seen: {} }]).map(
    (v) => v.id,
  ),
  [reloadedWriterId, legacyId],
  "A concurrent older-tab legacy write remains visible until observed",
);
const merged = await draftConflictHandler.resolve({
  realMasterState: {
    id: tabA,
    seq: 0,
    payload: JSON.stringify({
      id: tabA,
      session: "lead",
      device: "device",
      text: "Tab A text",
      updated: 20,
      seen: {},
    }),
  },
  newDocumentState: {
    id: tabB,
    seq: 0,
    payload: JSON.stringify({
      id: tabB,
      session: "lead",
      device: "device",
      text: "Tab B text",
      updated: 21,
      seen: {},
    }),
  },
});
assert.deepEqual(JSON.parse(merged.payload).alternatives, ["Tab A text"]);

const checkpointScope = `codex-drafts:${"f".repeat(32)}`;
storage.setItem(checkpointScope, JSON.stringify({ old: "Legacy checkpoint" }));
storage.setItem(
  localDraftKey(checkpointScope, "old"),
  JSON.stringify({
    version: 1,
    workspace: "f".repeat(32),
    session: "old",
    text: "Legacy checkpoint",
    deleted: false,
    source: "legacy",
    legacyBaseline: "Legacy checkpoint",
  }),
);
assert.deepEqual(migrateLegacyDrafts(checkpointScope), {});
const compactedLegacy = readLocalDraftRecord(checkpointScope, "old");
assert.equal(compactedLegacy.legacyBaseline, undefined);
assert.equal(
  compactedLegacy.legacyBaselineHash,
  legacyBaselineHash("old", "Legacy checkpoint"),
  "Older v1 checkpoint strings compact without changing the observed value",
);
assert.equal(
  legacyBaselineHash("s", null),
  "cfc323bb1d670d2756f1c1d120b3e52d6630c35ce03e7ffd823d28a662eb94e9",
  "Checkpoint uses a stable SHA-256 over framed session and absent value",
);
assert.equal(
  legacyBaselineHash("s", ""),
  "9c50e025b698ce7d5102ea62ecabecd3db46554078798248a4b1da66abd31edd",
  "Checkpoint distinguishes an empty draft from an absent value",
);
assert.notEqual(
  legacyBaselineHash("s", ""),
  legacyBaselineHash("other", ""),
  "Checkpoint fingerprints include the exact chat session",
);
assert.notEqual(
  legacyBaselineHash("s", "\ud800"),
  legacyBaselineHash("s", "\ufffd"),
  "Checkpoint hashing preserves exact UTF-16 code units, including unpaired surrogates",
);

const quotaStorage = new StorageFixture();
globalThis.localStorage = quotaStorage;
const quotaScope = `codex-drafts:${"e".repeat(32)}`;
const quotaLegacy = Object.fromEntries(
  Array.from({ length: 500 }, (_, index) => [
    `quota-chat-${index}`,
    "x".repeat(2000),
  ]),
);
quotaStorage.setItem(quotaScope, JSON.stringify(quotaLegacy));
const fiveMiB = 5 * 1024 * 1024;
quotaStorage.capacityBytes = fiveMiB;
const importedQuotaDrafts = migrateLegacyDrafts(quotaScope);
assert.deepEqual(importedQuotaDrafts, {});
assert.equal(Object.keys(readLocalDrafts(quotaScope)).length, 500);
assert.ok(
  quotaStorage.usedBytes < fiveMiB,
  "Compact checkpoints keep all 500 migrated drafts below the simulated 5 MiB quota",
);
const quotaUsedAfterMigration = quotaStorage.usedBytes;
const usageBytes = (predicate) =>
  [...quotaStorage.values].reduce(
    (bytes, [key, value]) =>
      bytes + (predicate(key) ? (key.length + value.length) * 2 : 0),
    0,
  );
const quotaStorageBreakdown = () => ({
  legacyAggregate: usageBytes((key) => key === quotaScope),
  perChatRecords: usageBytes((key) =>
    key.startsWith(`codex-chat-draft:${"e".repeat(32)}:`),
  ),
  pendingJournals: usageBytes((key) =>
    key.startsWith(`${quotaScope}:pending:`),
  ),
  otherKeys: usageBytes(
    (key) =>
      key !== quotaScope &&
      !key.startsWith(`codex-chat-draft:${"e".repeat(32)}:`) &&
      !key.startsWith(`${quotaScope}:pending:`),
  ),
});
const utf16BytesAfterMigration = quotaStorageBreakdown();
assert.equal(
  Object.values(utf16BytesAfterMigration).reduce(
    (sum, bytes) => sum + bytes,
    0,
  ),
  quotaUsedAfterMigration,
  "Quota byte artifact accounts for every actual persisted UTF-16 key and value",
);
const compactRecord = readLocalDraftRecord(quotaScope, "quota-chat-0");
assert.equal(compactRecord.legacyBaseline, undefined);
assert.match(compactRecord.legacyBaselineHash, /^[0-9a-f]{64}$/);
const quotaEdit = "e".repeat(2000);
writeLocalDraftRecord(quotaScope, "quota-chat-0", quotaEdit);
const quotaWriter = "d".repeat(36);
const quotaJournalKey = `${quotaScope}:pending:${quotaWriter}:quota-chat-0`;
const quotaJournalVersion = {
  id: `${"f".repeat(36)}:${quotaWriter}:quota-chat-0`,
  session: "quota-chat-0",
  device: "f".repeat(36),
  text: quotaEdit,
  updated: 1,
};
quotaStorage.setItem(quotaJournalKey, encodeDraftPayload(quotaJournalVersion));
const quotaUsedWithPendingJournal = quotaStorage.usedBytes;
const utf16BytesWithJournal = quotaStorageBreakdown();
assert.equal(
  Object.values(utf16BytesWithJournal).reduce((sum, bytes) => sum + bytes, 0),
  quotaUsedWithPendingJournal,
  "Pending journal keys and values are included in measured UTF-16 bytes",
);
const quotaFillerKey = "draft-quota-filler";
const quotaFillerChars = Math.floor(
  (fiveMiB - quotaStorage.usedBytes - quotaFillerKey.length * 2 - 256) / 2,
);
quotaStorage.setItem(quotaFillerKey, "z".repeat(quotaFillerChars));
const nearCapacityHeadroomBytes = fiveMiB - quotaStorage.usedBytes;
const oldQuotaDraft = readLocalDrafts(quotaScope)["quota-chat-0"];
assert.ok(nearCapacityHeadroomBytes <= 258);
assert.throws(
  () => writeLocalDraftRecord(quotaScope, "quota-chat-0", "y".repeat(10_000)),
  /QuotaExceededError/,
  "A near-capacity local draft write fails visibly instead of discarding data",
);
assert.equal(readLocalDrafts(quotaScope)["quota-chat-0"], quotaEdit);
assert.equal(oldQuotaDraft, quotaEdit);
assert.equal(
  JSON.parse(quotaStorage.getItem(quotaJournalKey)).text,
  quotaEdit,
  "A failed near-capacity write preserves the existing offline journal edit",
);
assert.deepEqual(JSON.parse(quotaStorage.getItem(quotaScope)), quotaLegacy);
quotaStorage.removeItem(quotaFillerKey);
const recoveredQuotaEdit = "y".repeat(10_000);
quotaStorage.setItem(
  quotaJournalKey,
  encodeDraftPayload({
    ...quotaJournalVersion,
    text: recoveredQuotaEdit,
    updated: 2,
  }),
);
writeLocalDraftRecord(quotaScope, "quota-chat-0", recoveredQuotaEdit);
assert.equal(readLocalDrafts(quotaScope)["quota-chat-0"], recoveredQuotaEdit);
assert.equal(
  JSON.parse(quotaStorage.getItem(quotaJournalKey)).text,
  recoveredQuotaEdit,
);
console.log(
  "Draft storage contract: PASS (partial migration, retry, idempotence, old-tab updates/removal, corruption recovery, tombstones, workspace scope, concurrent tab versions, and 5 MiB quota recovery).",
);
console.log(
  JSON.stringify({
    simulatedQuotaBytes: fiveMiB,
    migratedDrafts: 500,
    usedAfterMigrationBytes: quotaUsedAfterMigration,
    usedWithOnePendingJournalBytes: quotaUsedWithPendingJournal,
    nearCapacityHeadroomBytes,
    utf16BytesAfterMigration,
    utf16BytesWithOnePendingJournal: utf16BytesWithJournal,
  }),
);
