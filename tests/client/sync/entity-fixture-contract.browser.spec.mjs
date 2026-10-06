import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:http";
import {
  handleEntitySyncFixtureRequest,
  legacySnapshotRoute,
  entityPullFixtureForRequest,
  entityPullFixture,
  readTestState,
  readFixtureSyncContract,
  spawnFixture,
  stubEntityState,
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
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: { TOKEN_RATE_WORKER_COUNT: "1" },
    },
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
  const [identity, protocol, state] = await Promise.all([
    get("/api/sync/identity"),
    get("/api/sync/protocol"),
    readTestState(origin),
  ]);
  const snapshot = {
    token: state.token,
    stateDir: state.stateDir,
    threads: state.threads,
    chats: state.chats,
    edges: state.edges,
    runtime: state.runtime,
  };
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
  const capturedServerHead = await get(
    "/api/sync/pull?scope=state%3Aentities%3Av1&after=0&limit=500",
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
    assert.equal(
      typeof stubPull.documents.find((doc) => doc.id === id)._deleted,
      "boolean",
      `${id} stub tombstone marker`,
    );
  }
  assert.equal(stubPull.initialHigh, realPull.initialHigh, "fresh initialHigh");
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
  const serverMaxSeq = capturedServerHead.maxSeq;
  const stubHead = entityPullFixture(snapshot, {
    scope: "state:entities:v1",
    after: realPull.maxSeq,
    limit: 500,
    documents: capturedServerHead.documents,
  });
  assert.deepEqual(stubHead.documents, []);
  assert.equal(stubHead.checkpoint.seq, serverMaxSeq);

  const onePage = entityPullFixture(snapshot, { limit: 1, fresh: true });
  assert.equal(onePage.documents.length, 1, "entity page limit is honored");
  assert.equal(
    onePage.checkpoint.seq,
    onePage.documents[0].seq,
    "a full page checkpoints at its last returned document",
  );
  const resetPage = entityPullFixture(snapshot, {
    after: 1,
    floor: 2,
    maxSeq: realPull.maxSeq,
    reset: true,
  });
  assert.equal(resetPage.reset, true, "expired cursors request a reset");
  const priorityAgent = snapshot.runtime.agents[0];
  if (priorityAgent) {
    const priorityPage = entityPullFixture(snapshot, {
      fresh: true,
      priorityId: priorityAgent.id,
      limit: 1,
    });
    assert.equal(
      priorityPage.documents[0].id,
      `entity:agent:${priorityAgent.id}`,
      "priorityId places its agent first",
    );
  }

  const stream = await fetch(
    new URL(
      `/api/sync/stream?protocol=3&resources=${encodeURIComponent(JSON.stringify([{ kind: "state" }]))}`,
      origin,
    ),
  );
  assert.equal(stream.status, 200, "fixture stream remains open");
  const reader = stream.body.getReader();
  let streamText = "";
  const streamDeadline = Date.now() + 2000;
  while (
    Date.now() < streamDeadline &&
    !streamText.includes("event: token-rates")
  ) {
    let timeout;
    const chunk = await Promise.race([
      reader.read(),
      new Promise((resolve) => {
        timeout = setTimeout(() => resolve(null), 6000);
      }),
    ]);
    clearTimeout(timeout);
    if (!chunk || chunk.done) break;
    streamText += new TextDecoder().decode(chunk.value);
  }
  assert.match(streamText, /id: \d+/);
  assert.match(streamText, /event: token-rates/);
  assert.ok(
    streamText.indexOf("event: resources") <
      streamText.indexOf("event: token-rates"),
    "real token-rate frame follows the initial resource frame",
  );
  let openTimeout;
  const nextFrame = await Promise.race([
    reader.read(),
    new Promise((resolve) => {
      openTimeout = setTimeout(() => resolve(null), 1000);
    }),
  ]);
  clearTimeout(openTimeout);
  assert.notEqual(
    nextFrame?.done,
    true,
    "real stream remains open after initial frames",
  );
  await reader.cancel();

  const nodeSnapshot = structuredClone(snapshot);
  const streamServer = createServer((request, response) => {
    handleEntitySyncFixtureRequest(request, response, {
      snapshot: nodeSnapshot,
      workspaceId: identity.workspaceId,
    });
  });
  await new Promise((resolve) => streamServer.listen(0, "127.0.0.1", resolve));
  const stubStream = await fetch(
    `http://127.0.0.1:${streamServer.address().port}/api/sync/stream?protocol=3&resources=%5B%5D`,
  );
  assert.equal(stubStream.status, 200);
  const stubOrigin = `http://127.0.0.1:${streamServer.address().port}`;
  const nodeInitial = await fetch(
    new URL("/api/sync/pull?scope=state%3Aentities%3Av1&fresh=1", stubOrigin),
  ).then((response) => response.json());
  const removedNodeProject = nodeSnapshot.runtime.projects[0];
  nodeSnapshot.runtime.tasksHistoryLimit++;
  nodeSnapshot.runtime.projects = nodeSnapshot.runtime.projects.slice(1);
  const nodeChanged = await fetch(
    new URL(
      `/api/sync/pull?scope=state%3Aentities%3Av1&after=${nodeInitial.maxSeq}`,
      stubOrigin,
    ),
  ).then((response) => response.json());
  assert.ok(
    nodeChanged.documents.some(
      (document) =>
        document.id === "entity:workspace:current" &&
        document.seq > nodeInitial.maxSeq,
    ),
    "Node fixture assigns a new sequence to an in-place entity change",
  );
  assert.ok(
    nodeChanged.documents.some(
      (document) =>
        document.id === `entity:project:${removedNodeProject.id}` &&
        document._deleted === true &&
        document.seq > nodeInitial.maxSeq,
    ),
    "Node fixture emits a sequenced tombstone for an in-place removal",
  );
  for (const path of [
    "/api/sync/pull?after=-1",
    "/api/sync/pull?after=abc",
    "/api/sync/pull?after=1.5",
    "/api/sync/pull?scope=unknown",
  ]) {
    const [realResponse, stubResponse] = await Promise.all([
      fetch(new URL(path, origin)),
      fetch(new URL(path, stubOrigin)),
    ]);
    assert.equal(stubResponse.status, realResponse.status, `${path} status`);
    if (realResponse.status >= 400)
      assert.deepEqual(
        await stubResponse.json(),
        await realResponse.json(),
        `${path} error response matches the real server`,
      );
  }
  const stubReader = stubStream.body.getReader();
  let stubStreamText = "";
  const stubStreamDeadline = Date.now() + 6500;
  while (
    Date.now() < stubStreamDeadline &&
    !stubStreamText.includes("event: heartbeat")
  ) {
    const chunk = await stubReader.read();
    if (chunk.done) break;
    stubStreamText += new TextDecoder().decode(chunk.value);
  }
  assert.match(stubStreamText, /event: resources/);
  assert.match(stubStreamText, /id: \d+/);
  assert.match(stubStreamText, /event: token-rates/);
  assert.match(stubStreamText, /event: heartbeat/);
  assert.ok(
    stubStreamText.indexOf("event: resources") <
      stubStreamText.indexOf("event: token-rates") &&
      stubStreamText.indexOf("event: token-rates") <
        stubStreamText.indexOf("event: heartbeat"),
    "stub stream orders initial, token-rate and heartbeat frames",
  );
  const streamEvents = Object.fromEntries(
    [...stubStreamText.matchAll(/event: ([^\n]+)\ndata: ([^\n]+)/g)].map(
      ([, name, data]) => [name, JSON.parse(data)],
    ),
  );
  assert.equal(streamEvents.resources.protocol, protocol.protocolVersion);
  assert.equal(streamEvents.resources.workspaceId, identity.workspaceId);
  assert.equal(streamEvents.resources.reason, "initial");
  assert.equal(streamEvents["token-rates"].epoch, streamEvents.resources.epoch);
  assert.equal(
    streamEvents["token-rates"].revision,
    streamEvents.resources.revision,
  );
  assert.equal(streamEvents.heartbeat.epoch, streamEvents.resources.epoch);
  assert.ok(streamEvents.heartbeat.revision > streamEvents.resources.revision);
  await stubReader.cancel();
  streamServer.closeAllConnections();
  await new Promise((resolve) => streamServer.close(resolve));
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

test("shared entity stub converges on one matching stream", async ({
  page,
}) => {
  test.setTimeout(90000);
  const root = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const stateDir = await mkdtemp(join(tmpdir(), "studio-sync-stub-live-"));
  const child = spawnFixture(
    "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), stateDir],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: { TOKEN_RATE_WORKER_COUNT: "1" },
    },
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
  const snapshot = await readTestState(origin);
  const syncContract = await readFixtureSyncContract(origin);
  const requests = [];
  const pullScopes = [];
  const streamStatuses = [];
  const streamUrls = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/api/sync/")) requests.push(url.pathname);
    if (url.pathname === "/api/sync/pull")
      pullScopes.push(url.searchParams.get("scope"));
    if (url.pathname === "/api/sync/stream") streamUrls.push(url.href);
    if (legacySnapshotRoute.test(url.href)) requests.push(url.pathname);
  });
  page.on("response", (response) => {
    if (new URL(response.url()).pathname === "/api/sync/stream")
      streamStatuses.push(response.status());
  });
  try {
    const stub = await stubEntityState(page, snapshot, syncContract);
    await page.addInitScript(() => {
      const NativeEventSource = window.EventSource;
      window.__entityFixtureResourceEvents = [];
      window.EventSource = new Proxy(NativeEventSource, {
        construct(Target, args) {
          const stream = new Target(...args);
          stream.addEventListener("resources", (event) => {
            window.__entityFixtureResourceEvents.push(JSON.parse(event.data));
          });
          return stream;
        },
      });
    });
    await page.goto(origin);
    const routeParity = await page.evaluate(async () => {
      const paths = [
        "/api/sync/pull?after=-1",
        "/api/sync/pull?after=abc",
        "/api/sync/pull?after=1.5",
        "/api/sync/pull?scope=unknown",
      ];
      return Promise.all(
        paths.map(async (path) => {
          const response = await fetch(path);
          return { path, status: response.status };
        }),
      );
    });
    assert.deepEqual(
      routeParity.map(({ status }) => status),
      [200, 400, 400, 400],
      "browser page route validates pull requests like the real server",
    );
    await page.locator("#message").waitFor({ timeout: 30000 });
    // Let the renderer finish its initial resource union before measuring
    // reconnects; subscriptions are added as individual views mount.
    let stableCount = streamUrls.length;
    let unchangedSince = Date.now();
    const settleDeadline = Date.now() + 5000;
    while (Date.now() < settleDeadline && Date.now() - unchangedSince < 1000) {
      await page.waitForTimeout(100);
      if (streamUrls.length !== stableCount) {
        stableCount = streamUrls.length;
        unchangedSince = Date.now();
      }
    }
    const initialStreamConnections = stableCount;
    const settledStreamStart = streamUrls.length;
    await page.waitForTimeout(10000);
    const settledStreamUrls = streamUrls.slice(settledStreamStart);
    const streamConnections = settledStreamUrls.length;
    const repeatedStreamUrls = settledStreamUrls.filter(
      (url, index) => settledStreamUrls.indexOf(url) !== index,
    );
    const entityPulls = pullScopes.filter(
      (scope) => scope === "state:entities:v1",
    ).length;
    const snapshotReads = requests.filter((path) =>
      legacySnapshotRoute.test(path),
    ).length;
    console.log(
      "STUB_CONVERGENCE",
      JSON.stringify({
        observationMs: 10000,
        entityPulls,
        initialStreamConnections,
        streamConnections,
        repeatedStreamUrls,
        streamStatuses,
        snapshotReads,
      }),
    );
    assert.ok(entityPulls > 0, "page receives entity pulls");
    assert.ok(initialStreamConnections > 0, "page opens the matching stream");
    assert.equal(streamConnections, 0, "settled stream remains connected");
    assert.deepEqual(repeatedStreamUrls, [], "stream queries do not reconnect");
    assert.ok(streamStatuses.every((status) => status === 200));
    assert.equal(
      snapshotReads,
      0,
      "page does not use the legacy snapshot endpoint",
    );
    const initial = await page.evaluate(async () =>
      (
        await fetch("/api/sync/pull?scope=state%3Aentities%3Av1&fresh=1")
      ).json(),
    );
    assert.ok(snapshot.runtime.projects.length > 0, "fixture has a project");
    const removedProject = snapshot.runtime.projects[0];
    // Mutate the fixture snapshot directly, as the request fixtures do. This
    // does not commit through Runtime.db, so announce it through the stub.
    snapshot.runtime.tasksHistoryLimit++;
    snapshot.runtime.projects = snapshot.runtime.projects.slice(1);
    const notification = await stub.update(snapshot, {
      origin,
      token: snapshot.token,
      resources: [{ kind: "state" }],
    });
    assert.ok(notification, "state update receives a sync notification ack");
    // This explicitly tests the fixture notification path; the direct snapshot
    // mutation above bypasses the production entity publisher.
    await page.waitForFunction(() =>
      window.__entityFixtureResourceEvents?.some(
        (event) =>
          event.reason === "change" &&
          event.resources?.some((resource) => resource.kind === "state"),
      ),
    );
    const changed = await page.evaluate(
      async (after) =>
        (
          await fetch(
            `/api/sync/pull?scope=state%3Aentities%3Av1&after=${after}`,
          )
        ).json(),
      initial.maxSeq,
    );
    const changedWorkspace = changed.documents.find(
      (document) => document.id === "entity:workspace:current",
    );
    const removedDocument = changed.documents.find(
      (document) => document.id === `entity:project:${removedProject.id}`,
    );
    assert.ok(changedWorkspace, "changed workspace receives a new sequence");
    assert.ok(changedWorkspace.seq > initial.maxSeq);
    assert.equal(
      removedDocument?._deleted,
      true,
      "removed work gets a tombstone",
    );
    assert.ok(removedDocument.seq > initial.maxSeq);
  } finally {
    child.kill("SIGTERM");
  }
});
