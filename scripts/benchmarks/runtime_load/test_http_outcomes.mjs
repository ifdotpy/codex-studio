import assert from "node:assert/strict";
import test from "node:test";
import {
  HttpOutcomeTracker,
  isExpectedSyntheticUnavailable,
  isRetryableSnapshotDeferred,
  readRetryKey,
} from "./http_outcomes.mjs";
import { fetchRuntimeIdentity } from "./identity.mjs";

const pull = (scope, after = 0, limit = 100) =>
  `http://127.0.0.1/api/sync/pull?scope=${scope}&after=${after}&limit=${limit}`;
const deferredSnapshot = {
  error: "Runtime snapshot is temporarily unavailable; retry shortly.",
};

test("snapshot 503 is deferred only until same tab and read succeeds", () => {
  const outcomes = new HttpOutcomeTracker();
  outcomes.record({
    tab: 2,
    status: 503,
    url: pull("state", 7),
    method: "GET",
    detail: deferredSnapshot,
    atEpochMs: 100,
  });
  assert.equal(outcomes.failures.length, 0);
  assert.equal(outcomes.unrecoveredSnapshotReads().length, 1);
  outcomes.record({
    tab: 2,
    status: 204,
    url: pull("state", 7),
    method: "GET",
    atEpochMs: 450,
  });
  assert.equal(outcomes.unrecoveredSnapshotReads().length, 1);
  outcomes.record({
    tab: 2,
    status: 200,
    url: pull("state", 7),
    method: "GET",
    atEpochMs: 475,
  });
  assert.equal(outcomes.unrecoveredSnapshotReads().length, 0);
  assert.equal(outcomes.retryableSnapshotDeferred[0].recoveryLatencyMs, 375);
});

test("a different tab, scope, cursor, or route cannot recover a 503", () => {
  const outcomes = new HttpOutcomeTracker();
  outcomes.record({
    tab: 1,
    status: 503,
    url: pull("team", 7),
    method: "GET",
    detail: deferredSnapshot,
    atEpochMs: 100,
  });
  for (const sample of [
    { tab: 2, url: pull("team", 7) },
    { tab: 1, url: pull("state", 7) },
    { tab: 1, url: pull("team", 8) },
    { tab: 1, url: "http://127.0.0.1/api/state" },
  ])
    outcomes.record({ ...sample, status: 200, method: "GET", atEpochMs: 200 });
  assert.equal(outcomes.unrecoveredSnapshotReads().length, 1);
  assert.equal(outcomes.failures.length, 0);
});

test("unrecovered snapshot and non-snapshot 503s remain failures", () => {
  const outcomes = new HttpOutcomeTracker();
  outcomes.record({
    tab: 1,
    status: 503,
    url: pull("chat", 0),
    method: "GET",
    detail: deferredSnapshot,
    atEpochMs: 100,
  });
  outcomes.record({
    tab: 1,
    status: 503,
    url: "http://127.0.0.1/api/agent-chat?room=x",
    method: "GET",
    detail: deferredSnapshot,
    atEpochMs: 101,
  });
  assert.equal(outcomes.unrecoveredSnapshotReads().length, 1);
  assert.equal(outcomes.failures.length, 1);
});

test("runtime identity reconciles repeated entity versions and tombstones", async () => {
  const calls = [];
  const entity = (id, value, seq, deleted = false) => ({
    payload: JSON.stringify({ collection: "agent", id, value }),
    seq,
    _deleted: deleted,
  });
  const pages = new Map([
    [
      "0",
      {
        documents: [entity("lead", { id: "lead", mode: 1 }, 1)],
        checkpoint: { seq: 1 },
        maxSeq: 4,
      },
    ],
    [
      "1",
      {
        documents: [
          entity("lead", { id: "lead", mode: 2 }, 2),
          entity("gone", { id: "gone" }, 3),
        ],
        checkpoint: { seq: 3 },
        maxSeq: 4,
      },
    ],
    [
      "3",
      {
        documents: [entity("gone", {}, 4, true)],
        checkpoint: { seq: 4 },
        maxSeq: 4,
      },
    ],
  ]);
  const fetchStub = async (url, options = {}) => {
    calls.push({ url, options });
    if (url.endsWith("/api/session"))
      return Response.json({ token: "session-token" });
    if (url.endsWith("/api/desktop"))
      return Response.json({ stateDir: "/scratch/state" });
    const query = new URL(url).searchParams;
    assert.equal(query.get("scope"), "state:entities:v1");
    return Response.json(pages.get(query.get("after")));
  };
  assert.deepEqual(
    await fetchRuntimeIdentity("http://127.0.0.1:43123", fetchStub),
    {
      stateDir: "/scratch/state",
      threads: [{ id: "lead", mode: 2 }],
    },
  );
  assert.equal(calls.length, 5);
  assert(
    calls
      .slice(1)
      .every(
        ({ options }) => options.headers["X-Canvas-Token"] === "session-token",
      ),
  );
});

test("runtime identity caps a non-advancing entity page stream", async () => {
  let pullCount = 0;
  const fetchStub = async (url) => {
    if (url.endsWith("/api/session")) return Response.json({ token: "token" });
    if (url.endsWith("/api/desktop"))
      return Response.json({ stateDir: "/scratch/state" });
    pullCount += 1;
    return Response.json({
      documents: [],
      checkpoint: { seq: pullCount },
      maxSeq: 1000,
    });
  };
  await assert.rejects(
    fetchRuntimeIdentity("http://127.0.0.1:43123", fetchStub),
    /exceeded its 100-page limit at checkpoint 100/,
  );
  assert.equal(pullCount, 100);
});

test("one successful read recovers every prior retry of the same read", () => {
  const outcomes = new HttpOutcomeTracker();
  for (const atEpochMs of [100, 250])
    outcomes.record({
      tab: 3,
      status: 503,
      url: pull("transcript", 11),
      method: "GET",
      detail: deferredSnapshot,
      atEpochMs,
    });
  outcomes.record({
    tab: 3,
    status: 200,
    url: pull("transcript", 11),
    method: "GET",
    atEpochMs: 475,
  });
  assert.equal(outcomes.unrecoveredSnapshotReads().length, 0);
  assert.deepEqual(
    outcomes.retryableSnapshotDeferred.map(
      (attempt) => attempt.recoveryLatencyMs,
    ),
    [375, 225],
  );
});

test("expected synthetic account failure is only exact 400 and known reason", () => {
  const url = "http://127.0.0.1/api/models?account_key=bench-01";
  assert.equal(
    isExpectedSyntheticUnavailable(400, url, {
      error: "Unknown Codex account",
    }),
    true,
  );
  assert.equal(
    isExpectedSyntheticUnavailable(400, `${url}&workers=1`, {
      error: "No worker model catalog is available",
    }),
    true,
  );
  for (const sample of [
    [503, url, { error: "Unknown Codex account" }],
    [500, url, { error: "internal error" }],
    [400, url, { error: "invalid input" }],
    [400, url, { error: "No worker model catalog is available" }],
    [
      400,
      `${url}&workers=0`,
      { error: "No worker model catalog is available" },
    ],
    [
      400,
      "http://127.0.0.1/api/models?account_key=default",
      { error: "Unknown Codex account" },
    ],
    [
      400,
      "http://127.0.0.1/api/unrelated?account_key=bench-01",
      { error: "Unknown Codex account" },
    ],
  ])
    assert.equal(isExpectedSyntheticUnavailable(...sample), false);
});

test("read retry identity includes tab and complete read cursor", () => {
  assert.notEqual(
    readRetryKey(1, pull("team", 1)),
    readRetryKey(2, pull("team", 1)),
  );
  assert.notEqual(
    readRetryKey(1, pull("team", 1)),
    readRetryKey(1, pull("team", 2)),
  );
  assert.equal(readRetryKey(1, "http://127.0.0.1/api/state"), null);
});

test("only the known SnapshotDeferred error qualifies for retry accounting", () => {
  const url = pull("state", 0);
  assert.equal(
    isRetryableSnapshotDeferred(503, "GET", url, deferredSnapshot),
    true,
  );
  for (const sample of [
    [503, "GET", url, { error: "another 503" }],
    [503, "GET", "http://127.0.0.1/api/state", deferredSnapshot],
    [503, "POST", url, deferredSnapshot],
    [500, "GET", url, deferredSnapshot],
  ])
    assert.equal(isRetryableSnapshotDeferred(...sample), false);

  const outcomes = new HttpOutcomeTracker();
  outcomes.record({
    tab: 1,
    status: 503,
    url,
    method: "GET",
    detail: { error: "arbitrary pull failure" },
    atEpochMs: 100,
  });
  assert.equal(outcomes.retryableSnapshotDeferred.length, 0);
  assert.equal(outcomes.failures.length, 1);
});
