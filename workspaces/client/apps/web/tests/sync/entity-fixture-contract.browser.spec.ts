import { installEntityContractProbe } from "./entity-fixture-contract.fixture.ts";
import {
  fixtureProtocol,
  fixtureGet,
  pullPage,
  entityRows,
  fixtureObject,
  fixtureItem,
} from "./entity-fixture-protocol.ts";

import type {
  TestStateView,
  TranscriptPageResponse,
  SyncPullResponse,
} from "../playwright.ts";
import type { components } from "../../src/generated/api.ts";

import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
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
  rustUiFixture,
} from "../playwright.ts";

test("transcript fixture applies its sequence before the request cursor", async () => {
  const snapshot = {
    token: "fixture-token",
    stateDir: "/tmp/fixture",
    threads: [],
    chats: [],
    edges: [],
    runtime: {
      agents: [],
      rooms: [],
      tasks: [],
      monitors: [],
      complaints: [],
      projects: [],
      events: [],
      peerTeams: [],
      requests: [],
      rules: [],
      work: [],
      nativeNotices: [],
      sidebarOrder: { groups: null, revision: 0 },
      rateLimits: null,
      rateLimitsByAccount: {},
    },
  } satisfies TestStateView;
  const transcript = {
    items: [],
    truncated: false,
    agent: null,
    historyVersion: "0",
    nextAfterCursor: null,
    nextCursor: null,
    tail: null,
    unavailable: null,
  } satisfies TranscriptPageResponse;
  const firstPull = pullPage(
    entityPullFixtureForRequest(
      snapshot,
      "/api/sync/pull?scope=transcript%3Aagent-a&after=1&limit=100",
      { transcript, transcriptSeq: 2 },
    ),
  );
  assert.deepEqual(
    firstPull.documents.map(({ seq }) => seq),
    [2],
    "an advanced transcript sequence is eligible after the previous cursor",
  );
  assert.equal(firstPull.checkpoint.seq, 2);

  const unchangedPull = pullPage(
    entityPullFixtureForRequest(
      snapshot,
      "/api/sync/pull?scope=transcript%3Aagent-a&after=2&limit=100",
      { transcript, transcriptSeq: 2 },
    ),
  );
  assert.deepEqual(unchangedPull.documents, []);
  assert.equal(unchangedPull.checkpoint.seq, 2);
});

test("entity fixture contract matches the real sync backend", async () => {
  const stateDir = await mkdtemp(join(tmpdir(), "studio-sync-contract-"));
  const child = spawnFixture(process.execPath, [rustUiFixture, stateDir], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { FIXTURE_WORKER_COUNT: "1" },
  });
  const protocolClient = fixtureProtocol(child);
  const port = await protocolClient.port;
  const origin = `http://127.0.0.1:${port}`;
  const get = <Path extends import("../../src/apiTypes.ts").ApiGetPath>(
    path: Path,
    query = "",
  ) => fixtureGet(origin, path, query);
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

  const realPull = pullPage(
    await get(
      "/api/sync/pull",
      "?scope=state%3Aentities%3Av1&after=0&limit=500&fresh=1",
    ),
  );
  const capturedServerHead = pullPage(
    await get(
      "/api/sync/pull",
      "?scope=state%3Aentities%3Av1&after=0&limit=500",
    ),
  );
  assert.ok(typeof realPull.maxSeq === "number");
  const realMaxSeq = realPull.maxSeq;
  const stubPull = pullPage(
    entityPullFixture(snapshot, {
      scope: "state:entities:v1",
      after: 0,
      limit: 500,
      fresh: true,
      maxSeq: realMaxSeq,
    }),
  );
  const realById = new Map(
    entityRows(realPull.documents).map((doc) => [
      `entity:${doc.collection}:${doc.id}`,
      {
        collection: doc.collection,
        id: doc.id,
        value: "value" in doc ? fixtureObject(doc.value) : {},
      },
    ]),
  );
  const stubById = new Map(
    entityRows(stubPull.documents).map((doc) => [
      `entity:${doc.collection}:${doc.id}`,
      {
        collection: doc.collection,
        id: doc.id,
        value: "value" in doc ? fixtureObject(doc.value) : {},
      },
    ]),
  );
  assert.deepEqual(
    [...stubById.keys()].sort(),
    [...realById.keys()].sort(),
    "entity identifiers match the same fixture state",
  );
  for (const [id, value] of stubById) {
    const realValue = realById.get(id);
    assert.ok(realValue);
    assert.equal(typeof value.collection, "string", id);
    assert.equal(typeof value.id, "string", id);
    assert.deepEqual(
      Object.keys(realValue).sort(),
      Object.keys(value).sort(),
      `${id} envelope`,
    );
    if (value.collection === "event") {
      const expected = { ...value, value: { ...value.value } };
      const actual = {
        ...realById.get(id),
        value: { ...realValue.value },
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
        value: { ...realValue.value },
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
      typeof entityRows(realPull.documents).find(
        (doc) => `entity:${doc.collection}:${doc.id}` === id,
      )?.op,
      "string",
      `${id} operation`,
    );
    assert.equal(
      typeof entityRows(stubPull.documents).find(
        (doc) => `entity:${doc.collection}:${doc.id}` === id,
      )?.op,
      "string",
      `${id} stub operation`,
    );
  }
  assert.equal(stubPull.initialHigh, realPull.initialHigh, "fresh initialHigh");
  const workspaceEntity = stubById.get("entity:workspace:current");
  assert.ok(workspaceEntity);
  const workspace = workspaceEntity.value;
  assert.deepEqual(Object.keys(workspace).sort(), [
    "connected",
    "nativeNotices",
    "peerTeamsVersion",
    "projectOrganizationVersion",
    "sidebarOrder",
    "stateDir",
    "tasksHistoryLimit",
  ]);
  const serverMaxSeq = capturedServerHead.maxSeq;
  const stubHead = pullPage(
    entityPullFixture(snapshot, {
      scope: "state:entities:v1",
      after: realMaxSeq,
      limit: 500,
      documents: entityRows(capturedServerHead.documents),
    }),
  );
  assert.deepEqual(stubHead.documents, []);
  assert.equal(stubHead.checkpoint.seq, serverMaxSeq);

  const onePage = pullPage(
    entityPullFixture(snapshot, { limit: 1, fresh: true }),
  );
  assert.equal(onePage.documents.length, 1, "entity page limit is honored");
  assert.equal(
    onePage.checkpoint.seq,
    fixtureItem(onePage.documents, 0).seq,
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
    const priorityPage = pullPage(
      entityPullFixture(snapshot, {
        fresh: true,
        priorityId: priorityAgent.id,
        limit: 1,
      }),
    );
    assert.equal(
      `entity:${fixtureItem(entityRows(priorityPage.documents), 0).collection}:${fixtureItem(priorityPage.documents, 0).id}`,
      `entity:agent:${priorityAgent.id}`,
      "priorityId places its agent first",
    );
  }

  const stream = await fetch(
    new URL(
      `/api/sync/stream?protocol=4&resources=${encodeURIComponent(JSON.stringify([{ kind: "state" }]))}`,
      origin,
    ),
  );
  assert.equal(stream.status, 200, "fixture stream remains open");
  assert.ok(stream.body);
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
      new Promise<ReadableStreamReadResult<Uint8Array> | null>((resolve) => {
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
    new Promise<ReadableStreamReadResult<Uint8Array> | null>((resolve) => {
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
  await new Promise<void>((resolve) =>
    streamServer.listen(0, "127.0.0.1", resolve),
  );
  const address = streamServer.address();
  assert.ok(address && typeof address !== "string");
  const stubStream = await fetch(
    `http://127.0.0.1:${address.port}/api/sync/stream?protocol=4&resources=%5B%5D`,
  );
  assert.equal(stubStream.status, 200);
  const stubOrigin = `http://127.0.0.1:${address.port}`;
  const nodeInitial: SyncPullResponse = await fetch(
    new URL("/api/sync/pull?scope=state%3Aentities%3Av1&fresh=1", stubOrigin),
  ).then((response) => response.json());
  assert.ok(typeof nodeInitial.maxSeq === "number");
  const nodeInitialMax = nodeInitial.maxSeq;
  const removedNodeProject = fixtureItem(nodeSnapshot.runtime.projects, 0);
  assert.ok(typeof nodeSnapshot.runtime.tasksHistoryLimit === "number");
  nodeSnapshot.runtime.tasksHistoryLimit++;
  nodeSnapshot.runtime.projects = nodeSnapshot.runtime.projects.slice(1);
  const nodeChanged: SyncPullResponse = await fetch(
    new URL(
      `/api/sync/pull?scope=state%3Aentities%3Av1&after=${nodeInitial.maxSeq}`,
      stubOrigin,
    ),
  ).then((response) => response.json());
  assert.ok(
    entityRows(nodeChanged.documents).some(
      (document) =>
        document.collection === "workspace" &&
        document.id === "current" &&
        document.seq > nodeInitialMax,
    ),
    "Node fixture assigns a new sequence to an in-place entity change",
  );
  assert.ok(
    entityRows(nodeChanged.documents).some(
      (document) =>
        document.collection === "project" &&
        document.id === removedNodeProject.id &&
        document.op === "delete" &&
        document.seq > nodeInitialMax,
    ),
    "Node fixture emits a sequenced tombstone for an in-place removal",
  );
  // Detach stored entity values from live fixture objects, including nested DTOs.
  const nodeAgent = fixtureItem(nodeSnapshot.runtime.agents, 0);
  nodeAgent.activity = { phase: "writing", at: 1 };
  const nestedBaseline: SyncPullResponse = await fetch(
    new URL(
      `/api/sync/pull?scope=state%3Aentities%3Av1&after=${nodeChanged.maxSeq}`,
      stubOrigin,
    ),
  ).then((response) => response.json());
  const writing = entityRows(nestedBaseline.documents).find(
    (document) =>
      document.collection === "agent" && document.id === nodeAgent.id,
  );
  nodeAgent.activity.phase = "thinking";
  const nestedChange: SyncPullResponse = await fetch(
    new URL(
      `/api/sync/pull?scope=state%3Aentities%3Av1&after=${nestedBaseline.maxSeq}`,
      stubOrigin,
    ),
  ).then((response) => response.json());
  const thinking = entityRows(nestedChange.documents).find(
    (document) =>
      document.collection === "agent" && document.id === nodeAgent.id,
  );
  assert.ok(
    writing?.collection === "agent" &&
      writing.op === "upsert" &&
      writing.value.activity,
  );
  assert.ok(
    thinking?.collection === "agent" &&
      thinking.op === "upsert" &&
      thinking.value.activity,
  );
  assert.equal(
    writing.value.activity.phase,
    "writing",
    "an in-place nested edit does not rewrite its previous fixture document",
  );
  assert.equal(thinking.value.activity.phase, "thinking");
  assert.ok(
    thinking.seq > writing.seq,
    "an in-place nested edit receives a new sequence",
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
  assert.ok(stubStream.body);
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
  const streamEvents: Record<string, unknown> = Object.fromEntries(
    [...stubStreamText.matchAll(/event: ([^\n]+)\ndata: ([^\n]+)/g)].map(
      ([, name, data]) => {
        assert.ok(name && data);
        const frame: unknown = JSON.parse(data);
        return [name, frame];
      },
    ),
  );
  const resources = fixtureObject(streamEvents.resources);
  const tokenRates = fixtureObject(streamEvents["token-rates"]);
  const heartbeat = fixtureObject(streamEvents.heartbeat);
  assert.ok(
    typeof heartbeat.revision === "number" &&
      typeof resources.revision === "number",
  );
  assert.equal(resources.protocol, protocol.protocolVersion);
  assert.equal(resources.workspaceId, identity.workspaceId);
  assert.equal(resources.reason, "initial");
  assert.equal(tokenRates.epoch, resources.epoch);
  assert.equal(tokenRates.revision, resources.revision);
  assert.equal(heartbeat.epoch, resources.epoch);
  assert.ok(heartbeat.revision > resources.revision);
  await stubReader.cancel();
  streamServer.closeAllConnections();
  await new Promise<void>((resolve) => streamServer.close(() => resolve()));
  const realDrafts = pullPage(
    await get("/api/sync/pull", "?scope=drafts&after=0&limit=100"),
  );
  const stubDrafts = pullPage(
    entityPullFixtureForRequest(
      snapshot,
      "/api/sync/pull?scope=drafts&after=0&limit=100",
    ),
  );
  assert.deepEqual(stubDrafts.documents, realDrafts.documents);
  assert.equal(stubDrafts.checkpoint.seq, realDrafts.checkpoint.seq);
  const transcriptAgent = snapshot.threads[0]?.id;
  if (transcriptAgent) {
    const realHistory = await get(
      "/api/transcript",
      `?id=${encodeURIComponent(transcriptAgent)}`,
    );
    const scope = `transcript:${transcriptAgent}`;
    const realTranscript = pullPage(
      await get(
        "/api/sync/pull",
        `?scope=${encodeURIComponent(scope)}&after=0&limit=100`,
      ),
    );
    const stubTranscript = pullPage(
      entityPullFixture(snapshot, {
        scope,
        transcript: realHistory,
      }),
    );
    assert.equal(
      stubTranscript.documents.length,
      realTranscript.documents.length,
    );
    const stubDocument = fixtureItem(stubTranscript.documents, 0);
    const realDocument = fixtureItem(realTranscript.documents, 0);
    assert.ok("payload" in stubDocument && "payload" in realDocument);
    assert.deepEqual(
      JSON.parse(stubDocument.payload),
      JSON.parse(realDocument.payload),
    );
  }
});

test("shared entity stub converges on one matching stream", async ({
  page,
}) => {
  test.setTimeout(90000);
  const stateDir = await mkdtemp(join(tmpdir(), "studio-sync-stub-live-"));
  const child = spawnFixture(process.execPath, [rustUiFixture, stateDir], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { FIXTURE_WORKER_COUNT: "1" },
  });
  const protocolClient = fixtureProtocol(child);
  const port = await protocolClient.port;
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await readTestState(origin);
  const syncContract = await readFixtureSyncContract(origin);
  const requests: string[] = [];
  const pullScopes: Array<string | null> = [];
  const streamStatuses: number[] = [];
  const streamUrls: string[] = [];
  const subscriptionUpdates: components["schemas"]["SyncSubscriptionRequest"][] =
    [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/api/sync/")) requests.push(url.pathname);
    if (url.pathname === "/api/sync/pull")
      pullScopes.push(url.searchParams.get("scope"));
    if (url.pathname === "/api/sync/stream") streamUrls.push(url.href);
    if (url.pathname === "/api/sync/subscriptions")
      subscriptionUpdates.push(request.postDataJSON());
    if (legacySnapshotRoute.test(url.href)) requests.push(url.pathname);
  });
  page.on("response", (response) => {
    if (new URL(response.url()).pathname === "/api/sync/stream")
      streamStatuses.push(response.status());
  });
  try {
    const stub = await stubEntityState(page, snapshot, syncContract);
    await page.addInitScript(installEntityContractProbe);
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
      [400, 400, 400, 400],
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
    const streamLifecycle = await page.evaluate(
      () => window.__entityFixtureStreams,
    );
    const openStreams = streamLifecycle.filter(({ open }) => open);
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
        streamLifecycle: streamLifecycle.map(({ url, closeCalls, open }) => ({
          resources: new URL(url, origin).searchParams.get("resources"),
          closeCalls,
          open,
        })),
        openStreamCount: openStreams.length,
        streamStatuses,
        snapshotReads,
      }),
    );
    assert.ok(entityPulls > 0, "page receives entity pulls");
    assert.ok(initialStreamConnections > 0, "page opens the matching stream");
    assert.equal(
      openStreams.length,
      1,
      "resource union changes leave exactly one stream open",
    );
    assert.equal(
      initialStreamConnections,
      1,
      "resource union changes reuse the original v4 stream",
    );
    assert.equal(
      fixtureItem(streamLifecycle, 0).closeCalls,
      0,
      "live subscription updates do not close the stream",
    );
    assert.ok(
      subscriptionUpdates.length > 0,
      "mounted views update the live resource union",
    );
    const acknowledgements = await page.evaluate(
      () => window.__entityFixtureSubscriptionAcks,
    );
    assert.deepEqual(
      acknowledgements.map(({ gen }) => gen),
      subscriptionUpdates.map(({ gen }) => gen),
      "every resource update is acknowledged on the original stream",
    );
    assert.deepEqual(repeatedStreamUrls, [], "stream queries do not reconnect");
    assert.ok(streamStatuses.every((status) => status === 200));
    assert.equal(
      snapshotReads,
      0,
      "page does not use the legacy snapshot endpoint",
    );
    const initial = await page.evaluate(async (): Promise<SyncPullResponse> =>
      (
        await fetch("/api/sync/pull?scope=state%3Aentities%3Av1&fresh=1")
      ).json(),
    );
    assert.ok(snapshot.runtime.projects.length > 0, "fixture has a project");
    const removedProject = fixtureItem(snapshot.runtime.projects, 0);
    // Mutate the fixture snapshot directly, as the request fixtures do. This
    // does not commit through Runtime.db, so announce it through the stub.
    assert.ok(typeof snapshot.runtime.tasksHistoryLimit === "number");
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
      async (after): Promise<SyncPullResponse> =>
        (
          await fetch(
            `/api/sync/pull?scope=state%3Aentities%3Av1&after=${after}`,
          )
        ).json(),
      initial.maxSeq,
    );
    const changedWorkspace = entityRows(changed.documents).find(
      (document) =>
        document.collection === "workspace" && document.id === "current",
    );
    const removedDocument = entityRows(changed.documents).find(
      (document) =>
        document.collection === "project" && document.id === removedProject.id,
    );
    assert.ok(changedWorkspace, "changed workspace receives a new sequence");
    assert.ok(typeof initial.maxSeq === "number");
    assert.ok(changedWorkspace.seq > initial.maxSeq);
    assert.equal(
      removedDocument?.op,
      "delete",
      "removed work gets a tombstone",
    );
    assert.ok(removedDocument);
    assert.ok(removedDocument.seq > initial.maxSeq);
  } finally {
    child.kill("SIGTERM");
  }
});
