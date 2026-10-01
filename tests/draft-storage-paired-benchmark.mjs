// Paired storage-only comparison. Timings describe this Node fixture, not a browser.
import assert from "node:assert/strict";
import {
  migrateLegacyDrafts,
  writeLocalDraftRecord,
} from "../web/src/sync/draftStorage.ts";
import { encodeDraftPayload } from "../web/src/sync/draftPayload.ts";

class MemoryStorage {
  values = new Map();
  bytes = 0;
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
    this.bytes += String(value).length * 2;
    this.values.set(String(key), String(value));
  }
  removeItem(key) {
    this.values.delete(key);
  }
}

globalThis.localStorage = new MemoryStorage();
const workspace = "dddddddddddddddddddddddddddddddd";
const scope = `codex-drafts:${workspace}`;
const fixture = Object.fromEntries(
  Array.from({ length: 500 }, (_, index) => [
    `chat-${index}`,
    "x".repeat(2000),
  ]),
);
const old = async () => {
  const storage = new MemoryStorage();
  storage.setItem(scope, JSON.stringify(fixture));
  let bytes = 0;
  const times = [];
  for (let edit = 0; edit < 20; edit++) {
    const next = { ...fixture, "chat-0": `typed ${edit}` };
    const encoded = JSON.stringify(next);
    const journal = encodeDraftPayload({
      session: "chat-0",
      text: `typed ${edit}`,
      edit,
    });
    const start = performance.now();
    storage.setItem(scope, encoded);
    storage.setItem(`${scope}:pending:writer:chat-0`, journal);
    times.push(performance.now() - start);
    bytes += (encoded.length + journal.length) * 2;
  }
  return { bytes, times };
};
const current = () => {
  const storage = new MemoryStorage();
  globalThis.localStorage = storage;
  storage.setItem(scope, JSON.stringify(fixture));
  const legacyBytes = storage.bytes;
  migrateLegacyDrafts(scope);
  const migrationBytes = storage.bytes - legacyBytes;
  storage.bytes = 0;
  const times = [];
  for (let edit = 0; edit < 20; edit++) {
    const start = performance.now();
    writeLocalDraftRecord(scope, "chat-0", `typed ${edit}`);
    storage.setItem(
      `${scope}:pending:writer:chat-0`,
      encodeDraftPayload({ session: "chat-0", text: `typed ${edit}`, edit }),
    );
    times.push(performance.now() - start);
  }
  return { bytes: storage.bytes, migrationBytes, times };
};
const median = (values) => {
  const sorted = values.slice().sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
};
const rounds = [];
for (let round = 0; round < 2; round++) {
  const pair = round % 2 === 0 ? [old, current] : [current, old];
  const first = await pair[0]();
  const second = await pair[1]();
  rounds.push(round % 2 === 0 ? [first, second] : [second, first]);
}
const baselineBytes =
  rounds.reduce((sum, round) => sum + round[0].bytes, 0) / 2;
const currentBytes = rounds.reduce((sum, round) => sum + round[1].bytes, 0) / 2;
const migrationBytes = rounds[1][1].migrationBytes;
const baselineMedian = median(rounds.flatMap((round) => round[0].times));
const currentMedian = median(rounds.flatMap((round) => round[1].times));
assert.ok(currentBytes < baselineBytes / 100);
console.log(
  JSON.stringify({
    fixture: "500 drafts x 2000 chars, 20 edits",
    bytesPerEditBatch: {
      baselineAggregateMap: baselineBytes,
      currentPerChat: currentBytes,
    },
    oneTimeMigrationBytes: migrationBytes,
    nodeFixtureMedianMs: { baseline: baselineMedian, current: currentMedian },
    timingNote:
      "Storage-only in-memory Node observation; not a browser latency result.",
  }),
);
