import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  entityPullFixtureForRequest,
  entityPullFixture,
  readLegacySnapshotForS2Assertions,
  spawnFixture,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
} from "../playwright.mjs";

test("entity fixture contract matches the real sync backend", async () => {
  const root = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const stateDir = await mkdtemp(join(tmpdir(), "studio-sync-contract-"));
  const child = spawnFixture(
    "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), stateDir],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let stderr = "";
  child.stderr.on("data", (chunk) => (stderr += chunk));
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(Error(`fixture startup timeout: ${stderr}`)),
      30000,
    );
    child.stdout.once("data", (chunk) => {
      clearTimeout(timer);
      resolve(Number(String(chunk).trim()));
    });
    child.once("exit", () => {
      clearTimeout(timer);
      reject(Error(`fixture exited: ${stderr}`));
    });
  });
  const origin = `http://127.0.0.1:${port}`;
  const get = async (path) => {
    const response = await fetch(new URL(path, origin));
    assert.equal(response.status, 200, `${path} status`);
    return response.json();
  };
  const [identity, protocol, snapshot] = await Promise.all([
    get("/api/sync/identity"),
    get("/api/sync/protocol"),
    readLegacySnapshotForS2Assertions(origin),
  ]);
  assert.deepEqual(identity, syncIdentityFixture(identity.workspaceId));
  assert.deepEqual(
    protocol,
    syncProtocolFixture({
      unixSocket: protocol.capabilities.includes("unixSocket"),
    }),
  );

  const realPull = await get(
    "/api/sync/pull?scope=state%3Aentities%3Av1&after=0&limit=500&fresh=1",
  );
  const stubPull = entityPullFixture(snapshot, {
    scope: "state:entities:v1",
    after: 0,
    limit: 500,
    fresh: true,
    maxSeq: realPull.maxSeq,
  });
  const realById = new Map(
    realPull.documents.map((doc) => [doc.id, JSON.parse(doc.payload)]),
  );
  const stubById = new Map(
    stubPull.documents.map((doc) => [doc.id, JSON.parse(doc.payload)]),
  );
  assert.deepEqual(
    [...stubById.keys()].sort(),
    [...realById.keys()].sort(),
    "entity identifiers match the same fixture state",
  );
  for (const [id, value] of stubById) {
    assert.equal(typeof value.collection, "string", id);
    assert.equal(typeof value.id, "string", id);
    assert.deepEqual(
      Object.keys(realById.get(id)).sort(),
      Object.keys(value).sort(),
      `${id} envelope`,
    );
    if (value.collection === "event") {
      const expected = { ...value, value: { ...value.value } };
      const actual = {
        ...realById.get(id),
        value: { ...realById.get(id).value },
      };
      // The event delivery worker can advance status between the snapshot and
      // pull; its persisted entity status is the authoritative value.
      delete expected.value.status;
      delete actual.value.status;
      assert.deepEqual(expected, actual, `${id} stable event value matches`);
    } else if (value.collection === "agent") {
      const expected = { ...value, value: { ...value.value } };
      const actual = {
        ...realById.get(id),
        value: { ...realById.get(id).value },
      };
      // The fixture's active workers advance between independent snapshot and
      // pull transactions; compare their stable persisted identity/data here.
      for (const key of [
        "activity",
        "status",
        "turnId",
        "turnStatus",
        "inFlight",
      ]) {
        delete expected.value[key];
        delete actual.value[key];
      }
      assert.deepEqual(expected, actual, `${id} stable agent value matches`);
    } else {
      assert.deepEqual(
        value,
        realById.get(id),
        `${id} value matches the real entity`,
      );
    }
    assert.equal(
      typeof realPull.documents.find((doc) => doc.id === id)._deleted,
      "boolean",
      `${id} tombstone marker`,
    );
  }
  const workspace = stubById.get("entity:workspace:current").value;
  assert.deepEqual(Object.keys(workspace).sort(), [
    "connected",
    "nativeNotices",
    "peerTeamsVersion",
    "projectOrganizationVersion",
    "rateLimits",
    "rateLimitsByAccount",
    "sidebarOrder",
    "stateDir",
    "tasksHistoryLimit",
  ]);
  const head = await get(
    `/api/sync/pull?scope=state%3Aentities%3Av1&after=${realPull.maxSeq}&limit=500`,
  );
  const stubHead = entityPullFixture(snapshot, {
    scope: "state:entities:v1",
    after: realPull.maxSeq,
    limit: 500,
    maxSeq: realPull.maxSeq,
  });
  assert.deepEqual(stubHead.documents, []);
  assert.equal(stubHead.checkpoint.seq, head.checkpoint.seq);
  const realDrafts = await get("/api/sync/pull?scope=drafts&after=0&limit=100");
  const stubDrafts = entityPullFixtureForRequest(
    snapshot,
    "/api/sync/pull?scope=drafts&after=0&limit=100",
  );
  assert.deepEqual(stubDrafts.documents, realDrafts.documents);
  assert.equal(stubDrafts.checkpoint.seq, realDrafts.checkpoint.seq);
  const transcriptAgent = snapshot.threads[0]?.id;
  if (transcriptAgent) {
    const realHistory = await get(
      `/api/transcript?id=${encodeURIComponent(transcriptAgent)}`,
    );
    const scope = `transcript:${transcriptAgent}`;
    const realTranscript = await get(
      `/api/sync/pull?scope=${encodeURIComponent(scope)}&after=0&limit=100`,
    );
    const stubTranscript = entityPullFixture(snapshot, {
      scope,
      transcript: realHistory,
    });
    assert.equal(
      stubTranscript.documents.length,
      realTranscript.documents.length,
    );
    assert.deepEqual(
      JSON.parse(stubTranscript.documents[0].payload),
      JSON.parse(realTranscript.documents[0].payload),
    );
  }
});
