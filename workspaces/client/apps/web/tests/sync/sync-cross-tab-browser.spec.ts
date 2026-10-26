import type { GetResult } from "../../src/apiTypes.ts";
import type { Browser, BrowserContext, Page } from "@playwright/test";
import type { IncomingMessage, ServerResponse } from "node:http";
import type { Connect } from "vite";
import type { ResourceRef, ResourceChangeEvent } from "../playwright.ts";
import type {
  OutgoingTabMessage,
  ResourceVersion,
} from "../../src/sync/resourceEvents.ts";
import type { ResourceConnectionState } from "../../src/sync/resourceConnection.ts";
import { fixtureOrigin, fixtureResources } from "./helpers.ts";
// Same-origin Chromium proof: one elected sync stream fans out to eight tabs.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  test,
  expect,
} from "../playwright.ts";

test.use({ apiSchemaFixtureHeaders: false });

const runSyncCrossTabScenario = async (
  fixtureBrowser: Browser,
  checkResumeRace: boolean,
  checkDuplicateBaseline = false,
) => {
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  const workspaceId = "b".repeat(32);
  const otherWorkspaceId = "a".repeat(32);
  const workspaceFor = (req: IncomingMessage) =>
    req.headers.host?.startsWith("localhost:") ? otherWorkspaceId : workspaceId;
  const streams = new Set<ServerResponse>();
  let streamsOpened = 0;
  let entityPullRequests = 0;
  const streamRequestRecords: {
    openedAt: number;
    url: string | undefined;
    resourceQuery: string | null;
    resources: ResourceRef[];
    closedAt: number | undefined;
  }[] = [];
  const streamWorkspaces = new Map<ServerResponse, string>();
  const streamResources = new Map<ServerResponse, ResourceRef[]>();
  const revisions = new Map<string, number>();
  const resourceRevisions = new Map<string, Map<string, number>>();
  let unchangedReconnectBaseline = false;
  const setJsonHeaders = (response: ServerResponse) => {
    for (const [name, value] of Object.entries(
      apiSchemaHeaders({ "Content-Type": "application/json" }),
    ))
      response.setHeader(name, value);
  };
  const sendResourceEvent = (
    response: ServerResponse,
    currentWorkspace: string,
    reason: ResourceChangeEvent["reason"],
    resources: ResourceRef[],
  ) => {
    const revision = (revisions.get(currentWorkspace) || 0) + 1;
    revisions.set(currentWorkspace, revision);
    let known = resourceRevisions.get(currentWorkspace);
    if (!known) resourceRevisions.set(currentWorkspace, (known = new Map()));
    const resourceVersions = resources.map((resource) => {
      const key = JSON.stringify(resource);
      known.set(key, revision);
      return { revision, resource };
    });
    response.write(
      `event: resources\ndata: ${JSON.stringify({
        protocol: 4,
        workspaceId: currentWorkspace,
        epoch: "tab-epoch",
        revision,
        reason,
        resources,
        resourceVersions,
      } satisfies ResourceChangeEvent)}\n\n`,
    );
  };
  const sendSameVersionBaseline = (
    response: ServerResponse,
    currentWorkspace: string,
    reason: ResourceChangeEvent["reason"],
    resources: ResourceRef[],
  ) => {
    const known = resourceRevisions.get(currentWorkspace) || new Map();
    response.write(
      `event: resources\ndata: ${JSON.stringify({
        protocol: 4,
        workspaceId: currentWorkspace,
        epoch: "tab-epoch",
        revision: revisions.get(currentWorkspace) || 0,
        reason,
        resources,
        resourceVersions: resources.map((resource) => ({
          resource,
          revision: known.get(JSON.stringify(resource)) || 0,
        })),
      } satisfies ResourceChangeEvent)}\n\n`,
    );
  };
  const sendSameVersionBaselineToStreams = (currentWorkspace: string) => {
    for (const response of streams) {
      if (streamWorkspaces.get(response) !== currentWorkspace) continue;
      sendSameVersionBaseline(
        response,
        currentWorkspace,
        "reconnect",
        streamResources.get(response) || [],
      );
    }
  };
  const sendChange = (currentWorkspace: string, resource: ResourceRef) => {
    for (const response of streams) {
      if (streamWorkspaces.get(response) !== currentWorkspace) continue;
      const resources = streamResources.get(response) || [];
      if (
        resources.some(
          (entry) => JSON.stringify(entry) === JSON.stringify(resource),
        )
      )
        sendResourceEvent(response, currentWorkspace, "change", [resource]);
    }
  };
  const apiFixture: Connect.NextHandleFunction = (req, res, next) => {
    const path = new URL(req.url || "/", "http://localhost").pathname;
    if (path === "/sync-check") {
      res.setHeader("Content-Type", "text/html");
      res.end("<!doctype html><title>Cross tab sync</title>");
    } else if (path === "/api/sync/identity") {
      const currentWorkspace = workspaceFor(req);
      setJsonHeaders(res);
      res.end(
        JSON.stringify({
          workspaceId: currentWorkspace,
        } satisfies Pick<GetResult<"/api/sync/identity">, "workspaceId">),
      );
    } else if (path === "/api/sync/pull") {
      entityPullRequests++;
      const currentWorkspace = workspaceFor(req);
      setJsonHeaders(res);
      res.end(
        JSON.stringify({
          workspaceId: currentWorkspace,
          documents: [],
          checkpoint: { seq: 0 },
        } satisfies Pick<
          import("../playwright.ts").SyncPullResponse,
          "workspaceId" | "documents" | "checkpoint"
        >),
      );
    } else if (path === "/api/sync/stream") {
      const currentWorkspace = workspaceFor(req);
      streamsOpened++;
      res.writeHead(200, {
        ...apiSchemaHeaders({ "Content-Type": "text/event-stream" }),
        "Cache-Control": "no-cache",
        Connection: "keep-alive",
      });
      streams.add(res);
      streamWorkspaces.set(res, currentWorkspace);
      const resourceQuery = new URL(
        req.url || "/",
        "http://localhost",
      ).searchParams.get("resources");
      const resources = fixtureResources(resourceQuery);
      const requestRecord: (typeof streamRequestRecords)[number] = {
        openedAt: Date.now(),
        url: req.url,
        resourceQuery,
        resources,
        closedAt: undefined,
      };
      streamRequestRecords.push(requestRecord);
      if (
        new URL(req.url || "/", "http://localhost").searchParams.has(
          "apiSchema",
        )
      )
        res.write(apiSchemaHandshakeSse());
      streamResources.set(res, resources);
      if (unchangedReconnectBaseline && currentWorkspace === workspaceId) {
        unchangedReconnectBaseline = false;
        sendSameVersionBaseline(res, currentWorkspace, "initial", resources);
      } else sendResourceEvent(res, currentWorkspace, "initial", resources);
      res.on("close", () => {
        requestRecord.closedAt = Date.now();
        streams.delete(res);
        streamWorkspaces.delete(res);
        streamResources.delete(res);
      });
    } else next();
  };
  // The fixture must precede Vite's history fallback so API paths stay JSON.
  server.middlewares.stack.unshift({ route: "", handle: apiFixture });
  await server.listen();
  const browser = fixtureBrowser;
  const context = await browser.newContext();
  let noCoordination: Page | undefined;
  let otherWorkspaceContext: BrowserContext | undefined;
  const pages: Page[] = [];
  const pageAt = (index: number) => requiredEntry(pages, index);
  const waitFor = async (
    predicate: () => boolean | Promise<boolean>,
    message: string,
    timeoutMs = 10000,
  ) => {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      if (await predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
    assert.fail(message);
  };
  try {
    const origin = fixtureOrigin(server);
    const competingOwner = await context.newPage();
    await competingOwner.goto(`${origin}/sync-check`);
    const streamLockName = await competingOwner.evaluate(
      async (workspaceId) => {
        const { resourceCoordinationNames } =
          await import("/src/sync/resourceConnection.ts");
        return resourceCoordinationNames(location.origin, workspaceId).lock;
      },
      workspaceId,
    );
    await competingOwner.evaluate(async (name) => {
      if (!navigator.locks?.request)
        throw new Error("Web Locks are required for single-stream ownership");
      window.crossTabLockReady = new Promise<void>((resolve) => {
        window.crossTabResolveLockReady = resolve;
      });
      void navigator.locks.request(name, async (lock) => {
        if (!lock)
          throw new Error("Failed to establish competing stream lease");
        window.crossTabResolveLockReady();
        await new Promise<void>((resolve) => {
          window.crossTabReleaseStreamLock = resolve;
        });
      });
      await window.crossTabLockReady;
    }, streamLockName);
    await context.addInitScript(() => {
      window.crossTabDiagnostics = {
        sse: [],
        sources: [],
        sent: [],
        received: [],
      };
      const NativeSource = window.EventSource;
      window.EventSource = class extends NativeSource {
        constructor(...args: ConstructorParameters<typeof EventSource>) {
          super(...args);
          window.crossTabDiagnostics.sources.push(this);
          this.addEventListener("message", (event) =>
            window.crossTabDiagnostics.sse.push(String(event.data)),
          );
          for (const name of ["resources", "heartbeat", "token-rates"])
            this.addEventListener(name, (event) =>
              window.crossTabDiagnostics.sse.push(String(event.data)),
            );
        }
      };
      const NativeChannel = window.BroadcastChannel;
      window.BroadcastChannel = class extends NativeChannel {
        constructor(...args: ConstructorParameters<typeof BroadcastChannel>) {
          super(...args);
          this.addEventListener(
            "message",
            (event: MessageEvent<OutgoingTabMessage>) =>
              window.crossTabDiagnostics.received.push({
                ...event.data,
                __receivedAt: Date.now(),
              }),
          );
        }
        postMessage(data: OutgoingTabMessage) {
          window.crossTabDiagnostics.sent.push(data);
          return super.postMessage(data);
        }
      };
    });
    for (let i = 0; i < 8; i++) {
      const page = await context.newPage();
      pages.push(page);
      await page.goto(`${origin}/sync-check`);
      await page.evaluate(async () => {
        const client = await import("/src/sync/client.ts");
        const identity = await client.syncDatabase();
        window.crossTabWorkspace = identity.workspaceId;
        window.crossTabSyncDb = identity.db;
        window.crossTabCounts = { state: 0, transcripts: 0 };
        window.crossTabCountEvents = [];
        window.crossTabHasStateWatch = true;
        window.crossTabStopState = client.watchResourceChanges(
          { kind: "state" },
          (version) => {
            window.crossTabCounts.state++;
            window.crossTabCountEvents.push({
              resource: "state",
              version,
              received: window.crossTabDiagnostics.received.length,
              sse: window.crossTabDiagnostics.sse.length,
              at: performance.now(),
            });
          },
        );
        window.crossTabStopTranscript = client.watchResourceChanges(
          { kind: "transcript", agentId: "session-one" },
          (version) => {
            window.crossTabCounts.transcripts++;
            window.crossTabCountEvents.push({
              resource: "transcript",
              version,
              received: window.crossTabDiagnostics.received.length,
              sse: window.crossTabDiagnostics.sse.length,
              at: performance.now(),
            });
          },
        );
        window.crossTabStates = [];
        window.crossTabStopConnection = client.watchResourceConnection(
          (state) => window.crossTabStates.push(state),
        );
      });
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
    assert.equal(
      streams.size,
      0,
      "no elected tab may open SSE while another owner holds the stream lease",
    );
    await competingOwner.evaluate(() => window.crossTabReleaseStreamLock());
    await competingOwner.close();
    await waitFor(
      () => streams.size === 1 && streamsOpened >= 1,
      "the eight tabs did not settle on exactly one server stream",
      18_000,
    );
    await Promise.all(
      pages.map((page) =>
        page.waitForFunction(
          () =>
            window.crossTabWorkspace &&
            window.crossTabCounts &&
            window.crossTabCounts.state > 0 &&
            window.crossTabCounts.transcripts > 0,
          null,
          { timeout: 5000 },
        ),
      ),
    );
    const beforeForeignSchemaMessage = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.crossTabCounts }))),
    );
    await pageAt(7).evaluate(async (currentWorkspace) => {
      const { API_SCHEMA_HASH } = await import("/src/generated/apiSchema.ts");
      const { resourceCoordinationNames } =
        await import("/src/sync/resourceConnection.ts");
      const channel = new BroadcastChannel(
        resourceCoordinationNames(location.origin, currentWorkspace).channel,
      );
      channel.postMessage({
        kind: "resource-event",
        workspaceId: currentWorkspace,
        tabId: "foreign-schema-tab",
        apiSchemaHash: `${API_SCHEMA_HASH}-stale`,
        event: {
          protocol: 4,
          workspaceId: currentWorkspace,
          epoch: "foreign-schema-epoch",
          revision: 9000,
          reason: "change",
          resources: [{ kind: "state" }],
          resourceVersions: [{ resource: { kind: "state" }, revision: 9000 }],
        },
      });
      channel.close();
    }, workspaceId);
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.deepEqual(
      await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({ ...window.crossTabCounts })),
        ),
      ),
      beforeForeignSchemaMessage,
      "Tabs ignore channel messages from a different schema hash",
    );
    await new Promise((resolve) => setTimeout(resolve, 1400));
    const before = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.crossTabCounts }))),
    );
    const lateSubscriber = await context.newPage();
    await lateSubscriber.goto(`${origin}/sync-check`);
    await lateSubscriber.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.crossTabLateCount = 0;
      window.crossTabCounts = { state: 0, transcripts: 0 };
      window.crossTabCountEvents = [];
      window.crossTabHasStateWatch = false;
      window.crossTabStopLate = client.watchResourceChanges(
        { kind: "transcript", agentId: "session-one" },
        (version) => {
          window.crossTabLateCount++;
          window.crossTabCounts.transcripts++;
          window.crossTabCountEvents.push({
            resource: "transcript",
            version,
            received: window.crossTabDiagnostics.received.length,
            sse: window.crossTabDiagnostics.sse.length,
            at: performance.now(),
          });
        },
      );
    });
    await lateSubscriber.waitForFunction(() => window.crossTabLateCount > 0);
    pages.push(lateSubscriber);
    before.push(
      await lateSubscriber.evaluate(() => ({ ...window.crossTabCounts })),
    );
    sendChange(workspaceId, {
      kind: "transcript",
      agentId: "session-one",
    });
    try {
      await Promise.all(
        pages.map((page, index) =>
          page.waitForFunction(
            (prior) =>
              window.crossTabCounts.transcripts > prior.transcripts &&
              window.crossTabCounts.state === prior.state,
            requiredEntry(before, index),
            { timeout: 5000 },
          ),
        ),
      );
    } catch (error) {
      const diagnostics = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            counts: window.crossTabCounts,
            sse: window.crossTabDiagnostics.sse,
            sent: window.crossTabDiagnostics.sent,
            received: window.crossTabDiagnostics.received,
          })),
        ),
      );
      process.stderr.write(
        `fan-out diagnostic: ${JSON.stringify({ streamsOpened, diagnostics })}\n`,
      );
      throw error;
    }
    await pageAt(0).evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.crossTabQueueCallbacks = 0;
      window.crossTabStopQueueCallbacks = client.watchResourceChanges(
        { kind: "queue", agentId: "stable-queue" },
        () => window.crossTabQueueCallbacks++,
      );
    });
    await pageAt(2).evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.crossTabCostCallbacks = 0;
      window.crossTabStopCostCallbacks = client.watchResourceChanges(
        { kind: "costs" },
        () => window.crossTabCostCallbacks++,
      );
    });
    await Promise.all([
      pageAt(0).waitForFunction(() => window.crossTabQueueCallbacks > 0),
      pageAt(2).waitForFunction(() => window.crossTabCostCallbacks > 0),
    ]);
    const costBeforeUnrelated = await pageAt(2).evaluate(
      () => window.crossTabCostCallbacks,
    );
    sendChange(workspaceId, { kind: "costs" });
    await pageAt(2).waitForFunction(
      (prior) => window.crossTabCostCallbacks > prior,
      costBeforeUnrelated,
    );
    const queueFollower = pageAt(1);
    await queueFollower.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.crossTabLateQueueCallbacks = 0;
      window.crossTabStopLateQueue = client.watchResourceChanges(
        { kind: "queue", agentId: "stable-queue" },
        () => window.crossTabLateQueueCallbacks++,
      );
    });
    await queueFollower.waitForFunction(
      () => window.crossTabLateQueueCallbacks > 0,
      undefined,
      { timeout: 5000 },
    );
    const lateQueueBaseline = await queueFollower.evaluate(
      () => window.crossTabLateQueueCallbacks,
    );
    const lateQueueAfterUnrelated = await queueFollower.evaluate(
      () => window.crossTabLateQueueCallbacks,
    );
    assert.equal(
      lateQueueAfterUnrelated,
      lateQueueBaseline,
      "An unrelated resource event does not change the queue callback version",
    );
    sendChange(workspaceId, { kind: "queue", agentId: "stable-queue" });
    await queueFollower.waitForFunction(
      (prior) => window.crossTabLateQueueCallbacks > prior,
      lateQueueAfterUnrelated,
      { timeout: 5000 },
    );
    const stableQueueCallbacks = await queueFollower.evaluate(
      () => window.crossTabLateQueueCallbacks,
    );
    const costBeforeHeartbeat = await pageAt(2).evaluate(
      () => window.crossTabCostCallbacks,
    );
    sendChange(workspaceId, { kind: "costs" });
    await pageAt(2).waitForFunction(
      (prior) => window.crossTabCostCallbacks > prior,
      costBeforeHeartbeat,
    );
    await queueFollower.waitForTimeout(3500);
    assert.equal(
      await queueFollower.evaluate(() => window.crossTabLateQueueCallbacks),
      stableQueueCallbacks,
      "Peer subscription heartbeats do not replay unchanged resources after unrelated traffic",
    );
    const initialOwnerIndex = await Promise.any(
      pages.map(async (page, index) =>
        (await page.evaluate(() =>
          window.crossTabDiagnostics.sources.some(
            (source) => source.readyState === EventSource.OPEN,
          ),
        ))
          ? index
          : Promise.reject(),
      ),
    );
    const takeoverSubscriberIndex = initialOwnerIndex === 1 ? 0 : 1;
    await pageAt(takeoverSubscriberIndex).evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.crossTabTakeoverQueueCallbacks = 0;
      window.crossTabStopTakeoverQueue = client.watchResourceChanges(
        { kind: "queue", agentId: "takeover-unique-queue" },
        () => window.crossTabTakeoverQueueCallbacks++,
      );
    });
    await waitFor(
      () =>
        [...streamResources.values()].some((resources) =>
          resources.some(
            (resource) =>
              resource.kind === "queue" &&
              resource.agentId === "takeover-unique-queue",
          ),
        ),
      "the original owner did not learn the second tab's unique queue subscription",
    );
    await pageAt(0).evaluate(() => window.crossTabStopQueueCallbacks());
    await pageAt(2).evaluate(() => window.crossTabStopCostCallbacks());
    await queueFollower.evaluate(() => window.crossTabStopLateQueue());
    const streamsBeforeHidingOwner = streamsOpened;
    const requestsBeforeHidingOwner = streamRequestRecords.length;
    const subscriptionsBeforeTakeover = await Promise.all(
      pages.map((page) =>
        page.evaluate(() =>
          window.crossTabDiagnostics.received
            .filter(
              (
                message,
              ): message is Extract<
                typeof message,
                { kind: "subscriptions" }
              > => message.kind === "subscriptions",
            )
            .map(({ tabId, resources, tokenRates, reset }) => ({
              tabId,
              resources,
              tokenRates,
              reset,
            })),
        ),
      ),
    );
    await pageAt(initialOwnerIndex).evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: true,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    try {
      await pageAt(initialOwnerIndex).waitForFunction(
        () =>
          window.crossTabDiagnostics.sources.every(
            (source) => source.readyState === EventSource.CLOSED,
          ),
        null,
        { timeout: 5000 },
      );
    } catch (error) {
      const diagnostics = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            hidden: document.hidden,
            states: window.crossTabStates,
            sources: window.crossTabDiagnostics.sources.map((source) => ({
              readyState: source.readyState,
              url: source.url,
            })),
            sent: window.crossTabDiagnostics.sent,
            received: window.crossTabDiagnostics.received,
          })),
        ),
      );
      process.stderr.write(
        `owner release diagnostic: ${JSON.stringify({ streamsOpened, streamCount: streams.size, diagnostics })}\n`,
      );
      throw error;
    }
    const visibilityFailoverStarted = Date.now();
    try {
      await waitFor(
        async () =>
          streams.size === 1 &&
          (
            await Promise.all(
              pages
                .filter((_, index) => index !== initialOwnerIndex)
                .map((page) =>
                  page.evaluate(() =>
                    window.crossTabDiagnostics.sources.some(
                      (source) => source.readyState === EventSource.OPEN,
                    ),
                  ),
                ),
            )
          ).some(Boolean),
        "a visible peer did not take ownership after the active owner was hidden",
        5000,
      );
    } catch (error) {
      const diagnostics = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            hidden: document.hidden,
            states: window.crossTabStates,
            counts: window.crossTabCounts,
            sent: window.crossTabDiagnostics.sent.slice(-12),
            received: window.crossTabDiagnostics.received.slice(-12),
            sources: window.crossTabDiagnostics.sources.map((source) => ({
              readyState: source.readyState,
              url: source.url,
            })),
          })),
        ),
      );
      throw new Error(
        `${error instanceof Error ? error.message : String(error)}; ${JSON.stringify(
          {
            streamsOpened,
            streamsBeforeHidingOwner,
            streamCount: streams.size,
            diagnostics: diagnostics.map((entry) => ({
              hidden: entry.hidden,
              states: entry.states,
              openSources: entry.sources.filter(
                (source) => source.readyState === 1,
              ).length,
              sentKinds: entry.sent.map((message) => message.kind || "other"),
              receivedKinds: entry.received.map(
                (message) => message.kind || "other",
              ),
            })),
          },
        )}`,
      );
    }
    assert.ok(
      Date.now() - visibilityFailoverStarted <= 5000,
      "visible-peer ownership recovery exceeded five seconds",
    );
    const openSourcesByPage = await Promise.all(
      pages.map((page) =>
        page.evaluate(
          () =>
            window.crossTabDiagnostics.sources.filter(
              (source) => source.readyState === EventSource.OPEN,
            ).length,
        ),
      ),
    );
    assert.equal(
      openSourcesByPage.reduce((total, count) => total + count, 0),
      1,
      "exactly one EventSource remains open across tabs after takeover",
    );
    assert.equal(
      openSourcesByPage[initialOwnerIndex],
      0,
      "the hidden original owner's EventSource is closed",
    );
    assert.ok(
      openSourcesByPage.some(
        (count, index) => index !== initialOwnerIndex && count === 1,
      ),
      "the replacement EventSource belongs to a visible peer",
    );
    // Use the request-list boundary as the source of truth. Date.now() has
    // millisecond resolution and can timestamp an open just before the
    // takeover marker even though it belongs to this ownership transition.
    const takeoverRequests = streamRequestRecords.slice(
      requestsBeforeHidingOwner,
    );
    const takeoverConnectionCount = takeoverRequests.length;
    assert.equal(
      streamsOpened - streamsBeforeHidingOwner,
      takeoverConnectionCount,
      "every takeover EventSource open is recorded with its exact resource query",
    );
    assert.ok(takeoverRequests.length > 0);
    const hasUniqueQueue = (resources: ResourceRef[]) =>
      resources.some(
        (resource) =>
          resource.kind === "queue" &&
          resource.agentId === "takeover-unique-queue",
      );
    assert.ok(
      hasUniqueQueue(
        requiredEntry(takeoverRequests, takeoverRequests.length - 1).resources,
      ),
      `the final takeover query contains the subscribed queue: ${JSON.stringify(takeoverRequests)}`,
    );
    let takeoverSequence;
    for (let index = 0; index < takeoverRequests.length - 1; index++) {
      assert.ok(
        (requiredEntry(takeoverRequests, index).closedAt ??
          Number.POSITIVE_INFINITY) <=
          requiredEntry(takeoverRequests, index + 1).openedAt,
        `takeover never has two open streams at once: ${JSON.stringify(takeoverRequests)}`,
      );
    }
    if (takeoverRequests.length === 1) {
      takeoverSequence = "one request opens with the peer subscription union";
    } else if (takeoverRequests.length === 2) {
      const sameQuery =
        requiredEntry(takeoverRequests, 0).resourceQuery ===
        requiredEntry(takeoverRequests, 1).resourceQuery;
      if (sameQuery) {
        takeoverSequence =
          "duplicate identical union query; first stream was replaced";
      } else {
        assert.equal(
          hasUniqueQueue(requiredEntry(takeoverRequests, 0).resources),
          false,
          `the first query is local-only before the peer's queue is discovered: ${JSON.stringify(takeoverRequests)}`,
        );
        assert.ok(
          requiredEntry(takeoverRequests, 0).resources.every((resource) =>
            requiredEntry(takeoverRequests, 1).resources.some(
              (entry) => JSON.stringify(entry) === JSON.stringify(resource),
            ),
          ) &&
            requiredEntry(takeoverRequests, 0).resources.length <
              requiredEntry(takeoverRequests, 1).resources.length,
          `the replacement query extends the first query with discovered peer subscriptions: ${JSON.stringify(takeoverRequests)}`,
        );
        takeoverSequence =
          "local query replaced by discovered peer subscription union";
      }
    } else {
      takeoverSequence = `${takeoverRequests.length} sequential requests before the final union`;
    }
    assert.equal(streams.size, 1, "one SSE remains open after takeover");
    assert.deepEqual(
      [...streamResources.values()].map((resources) =>
        resources.map((resource) => JSON.stringify(resource)).sort(),
      ),
      [
        [
          JSON.stringify({ kind: "state" }),
          JSON.stringify({ kind: "transcript", agentId: "session-one" }),
          JSON.stringify({ kind: "queue", agentId: "takeover-unique-queue" }),
        ].sort(),
      ],
      "the sole active stream carries the new owner's local resources plus the discovered peer queue",
    );
    const takeoverOwnerIndex = openSourcesByPage.findIndex((count) => count);
    const peerQueueArrivedBeforeFirstOpen = await pageAt(
      takeoverOwnerIndex,
    ).evaluate(
      (firstOpenAt) =>
        window.crossTabDiagnostics.received.some(
          (message) =>
            message.kind === "subscriptions" &&
            message.__receivedAt <= firstOpenAt &&
            message.resources?.some(
              (resource) =>
                resource.kind === "queue" &&
                resource.agentId === "takeover-unique-queue",
            ),
        ),
      requiredEntry(takeoverRequests, 0).openedAt,
    );
    process.stdout.write(
      `TAKEOVER_STREAM_SEQUENCE ${JSON.stringify({
        sequence: takeoverSequence,
        takeoverOwnerIndex,
        ownerAlreadyKnewQueue:
          subscriptionsBeforeTakeover[takeoverOwnerIndex]?.some((message) =>
            hasUniqueQueue(message.resources),
          ) ?? false,
        peerQueueArrivedBeforeFirstOpen,
        subscriptionDiscoveryBeforeOpen: subscriptionsBeforeTakeover.map(
          (messages, index) => ({
            index,
            peerQueueAnnouncements: messages.filter((message) =>
              hasUniqueQueue(message.resources),
            ).length,
          }),
        ),
        subscriptionMessagesBeforeTakeover: subscriptionsBeforeTakeover.map(
          (messages, index) => ({
            index,
            queueAnnouncements: messages.filter((message) =>
              hasUniqueQueue(message.resources),
            ).length,
          }),
        ),
        opens: takeoverRequests.map(
          ({ openedAt, closedAt, url, resourceQuery }) => ({
            openedAt,
            closedAt,
            url,
            resourceQuery,
          }),
        ),
        openIntervalMs: requiredEntry(takeoverRequests, 1)
          ? requiredEntry(takeoverRequests, 1).openedAt -
            requiredEntry(takeoverRequests, 0).openedAt
          : undefined,
        activeStreamsAtEnd: streams.size,
      })}\n`,
    );
    process.stdout.write(
      `PASS ownership accounting: ${streamsBeforeHidingOwner} streams before hide, ${takeoverConnectionCount} requests during takeover, ${streamsOpened} total\n`,
    );
    await waitFor(
      () =>
        [...streamResources.values()].some((resources) =>
          resources.some(
            (resource) =>
              resource.kind === "queue" &&
              resource.agentId === "takeover-unique-queue",
          ),
        ),
      "the replacement owner did not rediscover the surviving peer's unique queue",
    );
    const takeoverQueueBefore = await pageAt(takeoverSubscriberIndex).evaluate(
      () => window.crossTabTakeoverQueueCallbacks,
    );
    sendChange(workspaceId, {
      kind: "queue",
      agentId: "takeover-unique-queue",
    });
    await pageAt(takeoverSubscriberIndex).waitForFunction(
      (previous) => window.crossTabTakeoverQueueCallbacks > previous,
      takeoverQueueBefore,
      { timeout: 5000 },
    );
    const takeoverQueueAfterChange = await pageAt(
      takeoverSubscriberIndex,
    ).evaluate(() => window.crossTabTakeoverQueueCallbacks);
    await pageAt(takeoverSubscriberIndex).waitForTimeout(3500);
    assert.equal(
      await pageAt(takeoverSubscriberIndex).evaluate(
        () => window.crossTabTakeoverQueueCallbacks,
      ),
      takeoverQueueAfterChange,
      "quiet peer heartbeats do not refetch the surviving queue after takeover",
    );
    if (checkResumeRace) {
      const resumedOwnerIndex = await Promise.any(
        pages.map(async (page, index) =>
          (await page.evaluate(() =>
            window.crossTabDiagnostics.sources.some(
              (source) => source.readyState === EventSource.OPEN,
            ),
          ))
            ? index
            : Promise.reject(),
        ),
      );
      const followerIndex = pages.findIndex(
        (_, index) =>
          index !== resumedOwnerIndex && index !== initialOwnerIndex,
      );
      const follower = pageAt(followerIndex);
      await follower.evaluate(async () => {
        Object.defineProperty(document, "hidden", {
          configurable: true,
          value: true,
        });
        document.dispatchEvent(new Event("visibilitychange"));
      });
      await follower.waitForTimeout(100);
      await follower.evaluate(async () => {
        Object.defineProperty(document, "hidden", {
          configurable: true,
          value: false,
        });
        document.dispatchEvent(new Event("visibilitychange"));
        const client = await import("/src/sync/client.ts");
        window.crossTabResumedQueueCount = 0;
        window.crossTabStopResumedQueue = client.watchResourceChanges(
          { kind: "queue", agentId: "resumed-follower" },
          () => window.crossTabResumedQueueCount++,
        );
      });
      await waitFor(
        () =>
          [...streamResources.values()].some((resources) =>
            resources.some(
              (resource) =>
                resource.kind === "queue" &&
                resource.agentId === "resumed-follower",
            ),
          ),
        "the resumed follower's new ref was not added to the owner's stream",
      );
      await follower.waitForFunction(
        () => window.crossTabResumedQueueCount > 0,
        undefined,
        { timeout: 5000 },
      );
      await new Promise((resolve) => setTimeout(resolve, 11_000));
      assert.ok(
        [...streamResources.values()].some((resources) =>
          resources.some(
            (resource) =>
              resource.kind === "queue" &&
              resource.agentId === "resumed-follower",
          ),
        ),
        "the resumed follower kept its ref advertised past peer expiry",
      );
      const resumedBefore = await follower.evaluate(
        () => window.crossTabResumedQueueCount,
      );
      sendChange(workspaceId, {
        kind: "queue",
        agentId: "resumed-follower",
      });
      await follower.waitForFunction(
        (previous) => window.crossTabResumedQueueCount > previous,
        resumedBefore,
        { timeout: 5000 },
      );
      await follower.evaluate(() => window.crossTabStopResumedQueue());
      await pageAt(1).evaluate(() => window.crossTabStopTakeoverQueue());
      await pageAt(initialOwnerIndex).evaluate(() => {
        Object.defineProperty(document, "hidden", {
          configurable: true,
          value: false,
        });
        document.dispatchEvent(new Event("visibilitychange"));
      });
      await new Promise((resolve) => setTimeout(resolve, 100));
      let duplicateWaitError;
      try {
        await waitFor(
          () => streams.size === 1,
          "a resumed former owner left a duplicate stream after close propagation",
          1500,
        );
      } catch (error) {
        duplicateWaitError = error;
      }
      const resumedOwnerConvergence = {
        waitPassed: !duplicateWaitError,
        streams: [...streamResources.values()],
        resumedOwnerSources: await pageAt(initialOwnerIndex).evaluate(() =>
          window.crossTabDiagnostics.sources.map((source) => ({
            readyState: source.readyState,
            url: source.url,
          })),
        ),
      };
      process.stderr.write(
        `RESUMED_OWNER_CONVERGENCE ${JSON.stringify(resumedOwnerConvergence)}\n`,
      );
      if (!duplicateWaitError) {
        assert.deepEqual(
          [...streamResources.values()],
          [[{ kind: "transcript", agentId: "session-one" }, { kind: "state" }]],
          "the converged resumed-owner stream retains its expected transcript and state resources",
        );
      }
      if (duplicateWaitError && streams.size > 1) {
        const duplicateStreamCount = streams.size;
        const resourcesDuringOverlap = [...streamResources.values()];
        const callbacksBefore = await Promise.all(
          pages.map((page) =>
            page.evaluate(() => ({ ...window.crossTabCounts })),
          ),
        );
        const pullsBeforeChange = entityPullRequests;
        sendChange(workspaceId, { kind: "state" });
        await new Promise((resolve) => setTimeout(resolve, 500));
        const callbacksAfterChange = await Promise.all(
          pages.map((page) =>
            page.evaluate(() => ({ ...window.crossTabCounts })),
          ),
        );
        const pullsAfterChange = entityPullRequests;
        await new Promise((resolve) => setTimeout(resolve, 10_000));
        const streamsAfterTenSeconds = streams.size;
        process.stderr.write(
          `RESUMED_OWNER_DUPLICATE_EFFECT ${JSON.stringify({
            duplicateStreamCount,
            resourcesDuringOverlap,
            streamsAfterTenSeconds,
            resources: [...streamResources.values()],
            resumedOwnerSources: await pageAt(initialOwnerIndex).evaluate(() =>
              window.crossTabDiagnostics.sources.map((source) => ({
                readyState: source.readyState,
                url: source.url,
              })),
            ),
            callbacksBefore,
            callbacksAfterChange,
            pullDeltaDuringOverlap: pullsAfterChange - pullsBeforeChange,
            observedAfterMs: 11_500,
          })}\n`,
        );
      }
      const resumedOwnerDiagnostic = await Promise.all(
        pages.map((page, index) =>
          page.evaluate(
            (index) => ({
              index,
              hidden: document.hidden,
              states: window.crossTabStates,
              sources: window.crossTabDiagnostics.sources.map((source) => ({
                readyState: source.readyState,
                url: source.url,
              })),
              received: window.crossTabDiagnostics.received.slice(-6),
            }),
            index,
          ),
        ),
      );
      assert.equal(
        streams.size,
        1,
        `a resumed former owner cannot duplicate the current SSE: ${JSON.stringify({ streamsOpened, streamResources: [...streamResources.values()], resumedOwnerDiagnostic })}`,
      );
      if (duplicateWaitError) throw duplicateWaitError;
      return;
    }
    await pageAt(1).evaluate(() => window.crossTabStopTakeoverQueue());
    const beforeOffline = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.crossTabCounts }))),
    );
    const hadStateWatch = await Promise.all(
      pages.map((page) => page.evaluate(() => window.crossTabHasStateWatch)),
    );
    await context.setOffline(true);
    await waitFor(
      () => streams.size === 0,
      "the owner stream remained open after all tabs went offline",
    );
    unchangedReconnectBaseline = true;
    await context.setOffline(false);
    await waitFor(
      () => streams.size === 1,
      "the owner stream did not reconnect when the profile returned online",
      5000,
    );
    try {
      await Promise.all(
        pages.map((page, index) =>
          page.waitForFunction(
            ({ prior, needsState }) =>
              (!needsState || window.crossTabCounts.state > prior.state) &&
              window.crossTabCounts.transcripts > prior.transcripts,
            {
              prior: requiredEntry(beforeOffline, index),
              needsState: index < 8,
            },
            { timeout: 5000 },
          ),
        ),
      );
    } catch (error) {
      const diagnostics = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            hidden: document.hidden,
            counts: window.crossTabCounts,
            states: window.crossTabStates,
            sse: window.crossTabDiagnostics.sse.slice(-5),
            received: window.crossTabDiagnostics.received.slice(-8),
          })),
        ),
      );
      process.stderr.write(
        `resume baseline diagnostic: ${JSON.stringify({ beforeOffline, streamsOpened, diagnostics })}\n`,
      );
      throw error;
    }
    const afterRecovery = await Promise.all(
      pages.map((page) =>
        page.evaluate(() => ({
          counts: { ...window.crossTabCounts },
          countEvents: window.crossTabCountEvents.slice(-8),
          received: window.crossTabDiagnostics.received.slice(-8),
          sse: window.crossTabDiagnostics.sse.slice(-8),
        })),
      ),
    );
    if (checkDuplicateBaseline) {
      for (let index = 0; index < pages.length; index++) {
        const expected = {
          state:
            requiredEntry(beforeOffline, index).state +
            Number(hadStateWatch[index]),
          transcripts: requiredEntry(beforeOffline, index).transcripts + 1,
        };
        assert.deepEqual(
          requiredEntry(afterRecovery, index).counts,
          expected,
          `tab ${index} reconciles its unchanged same-version resume baseline exactly once`,
        );
      }
      const duplicateWindowStartedAt = Date.now();
      sendSameVersionBaselineToStreams(workspaceId);
      await pageAt(0).waitForTimeout(3500);
      const duplicateWindowEndedAt = Date.now();
      const afterDuplicateAudit = await Promise.all(
        pages.map((page) =>
          page.evaluate(
            ({ startedAt, endedAt }) => ({
              counts: { ...window.crossTabCounts },
              frames: window.crossTabDiagnostics.received
                .filter(
                  (
                    message,
                  ): message is Extract<
                    typeof message,
                    { kind: "resource-event" }
                  > =>
                    message.kind === "resource-event" &&
                    message.__receivedAt >= startedAt &&
                    message.__receivedAt <= endedAt,
                )
                .map(({ event, __receivedAt }) => ({
                  at: __receivedAt,
                  reason: event.reason,
                  revision: event.revision,
                  epoch: event.epoch,
                  resources: event.resources,
                  resourceVersions: event.resourceVersions,
                })),
            }),
            {
              startedAt: duplicateWindowStartedAt,
              endedAt: duplicateWindowEndedAt,
            },
          ),
        ),
      );
      const afterDuplicateAndHeartbeats = afterDuplicateAudit.map(
        ({ counts }) => counts,
      );
      const recoveredCounts = afterRecovery.map(({ counts }) => counts);
      assert.deepEqual(
        afterDuplicateAndHeartbeats,
        recoveredCounts,
        `duplicate unchanged baselines and peer heartbeats do not refetch resources: ${JSON.stringify(
          afterDuplicateAudit.map((page, index) => ({
            tab: index,
            before: recoveredCounts[index],
            after: page.counts,
            frames: page.frames,
          })),
        )}`,
      );
      return;
    }
    const streamOwnerIndex = await Promise.any(
      pages.map(async (page, index) =>
        (await page.evaluate(() =>
          window.crossTabDiagnostics.sources.some(
            (source) => source.readyState === EventSource.OPEN,
          ),
        ))
          ? index
          : Promise.reject(),
      ),
    );
    const replacementFollowerIndex = streamOwnerIndex === 0 ? 1 : 0;
    await pageAt(replacementFollowerIndex).evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: false,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.equal(streams.size, 1, "one owner stream remains before failover");
    assert.equal(
      await pageAt(streamOwnerIndex).evaluate(
        async (name) =>
          ((await navigator.locks.query()).held || []).some(
            (lock) => lock.name === name,
          ),
        streamLockName,
      ),
      true,
      "the selected stream owner still holds its workspace lock immediately before close",
    );
    const failoverStarted = Date.now();
    const streamsOpenedBeforeFailover = streamsOpened;
    await pageAt(streamOwnerIndex).close();
    pages.splice(streamOwnerIndex, 1);
    await waitFor(
      () =>
        streams.size === 1 &&
        streamsOpened > streamsOpenedBeforeFailover &&
        [...streams].every(
          (response) => streamWorkspaces.get(response) === workspaceId,
        ),
      "a replacement owner did not open its stream within five seconds",
      5000,
    );
    const failoverMs = Date.now() - failoverStarted;
    assert.ok(failoverMs <= 5000, "owner failover exceeded five seconds");
    assert.equal(
      streams.size,
      1,
      "only one stream should remain after failover",
    );
    const workspaceOwnerIndex = await Promise.any(
      pages.map(async (page, index) => {
        await page.waitForFunction(
          () =>
            window.crossTabDiagnostics.sources.some(
              (source) => source.readyState === EventSource.OPEN,
            ),
          undefined,
          { timeout: 5000 },
        );
        return index;
      }),
    );
    await pageAt(workspaceOwnerIndex).evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: false,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    otherWorkspaceContext = await browser.newContext();
    const isolatedPage = await otherWorkspaceContext.newPage();
    await isolatedPage.goto(`${fixtureOrigin(server, "localhost")}/sync-check`);
    await isolatedPage.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.crossTabIdentity = (await client.syncDatabase()).workspaceId;
      window.crossTabStop = client.watchSyncInvalidations("state", () => {});
    });
    try {
      await waitFor(
        () => streams.size === 2,
        "a second workspace did not get its own elected stream",
      );
    } catch (error) {
      const diagnostics = await Promise.all(
        [...pages, isolatedPage].map((page, index) =>
          page.evaluate(
            (index) => ({
              index,
              workspace: window.crossTabWorkspace,
              identity: window.crossTabIdentity,
              states: window.crossTabStates,
              sources: (window.crossTabDiagnostics?.sources || []).map(
                (source) => ({
                  readyState: source.readyState,
                  url: source.url,
                }),
              ),
              received: window.crossTabDiagnostics?.received.slice(-5) || [],
            }),
            index,
          ),
        ),
      );
      throw new Error(
        `${error instanceof Error ? error.message : String(error)}; workspace stream diagnostic: ${JSON.stringify({ streamsOpened, activeWorkspaces: [...streams].map((response) => streamWorkspaces.get(response)), diagnostics })}`,
      );
    }
    assert.equal(
      await isolatedPage.evaluate(() => window.crossTabIdentity),
      otherWorkspaceId,
    );
    const beforeWorkspaceMove = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.crossTabCounts }))),
    );
    for (const response of streams)
      if (streamWorkspaces.get(response) === workspaceId)
        sendResourceEvent(response, workspaceId, "workspace", [
          { kind: "state" },
          { kind: "transcript", agentId: "session-one" },
        ]);
    try {
      await Promise.all(
        pages.map((page, index) =>
          page.waitForFunction(
            (prior) =>
              (!window.crossTabHasStateWatch ||
                window.crossTabCounts.state > prior.state) &&
              window.crossTabCounts.transcripts > prior.transcripts,
            requiredEntry(beforeWorkspaceMove, index),
            { timeout: 4500 },
          ),
        ),
      );
    } catch (error) {
      const diagnostics = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            counts: window.crossTabCounts,
            states: window.crossTabStates,
            sse: window.crossTabDiagnostics.sse.slice(-8),
            sent: window.crossTabDiagnostics.sent.slice(-5),
            received: window.crossTabDiagnostics.received.slice(-8),
          })),
        ),
      );
      process.stderr.write(
        `workspace reset diagnostic: ${JSON.stringify({ beforeWorkspaceMove, streamsOpened, streamResources: [...streamResources.values()], diagnostics })}\n`,
      );
      throw error;
    }
    await otherWorkspaceContext.close();
    otherWorkspaceContext = undefined;
    await context.close();
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(Number(streams.size)).toBe(0);
    noCoordination = await browser.newPage();
    await noCoordination.addInitScript(() => {
      Object.defineProperty(window, "BroadcastChannel", { value: undefined });
    });
    await noCoordination.goto(`${origin}/sync-check`);
    let generationPollCount = 0;
    noCoordination.on("request", (request) => {
      if (request.url().includes("/api/sync/generations"))
        generationPollCount++;
    });
    await noCoordination.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.crossTabStop = client.watchResourceChanges(
        { kind: "state" },
        () => {},
      );
    });
    await waitFor(
      () => streams.size === 1,
      "independent EventSource did not remain available when BroadcastChannel was unavailable",
    );
    assert.equal(
      generationPollCount,
      0,
      "uncoordinated mode must not poll generations",
    );
    await noCoordination.close();
    process.stdout.write(
      `PASS: 8 tabs, 1 active SSE, ${streamsOpened} total stream requests, scoped transcript fan-out, failover ${failoverMs}ms\n`,
    );
  } finally {
    await noCoordination?.close();
    await otherWorkspaceContext?.close().catch(() => {});
    await context.close().catch(() => {});
    await server.close();
  }
};

test("sync-cross-tab-browser @performance", async ({ browser }) => {
  test.setTimeout(120_000);
  await runSyncCrossTabScenario(browser, false);
});

test("duplicate unchanged baselines and peer heartbeats do not refetch resources @performance", async ({
  browser,
}) => {
  test.fixme(
    true,
    "An intermittent reconnect/initial frame burst notifies state and transcript watchers again at unchanged resource versions",
  );
  test.setTimeout(120_000);
  await runSyncCrossTabScenario(browser, false, true);
});

test("resumed former owner closes its duplicate stream @performance", async ({
  browser,
}) => {
  test.fixme(
    true,
    "The full cross-tab takeover and resume sequence intermittently leaves a duplicate stream on the resumed former owner",
  );
  test.setTimeout(180_000);
  await runSyncCrossTabScenario(browser, true);
});

function requiredEntry<T>(items: readonly T[], index: number): T {
  const item = items[index];
  assert.ok(item !== undefined, `Missing fixture entry at index ${index}`);
  return item;
}

type CrossTabCounts = { state: number; transcripts: number };
declare global {
  interface Window {
    crossTabLockReady: Promise<void>;
    crossTabResolveLockReady: () => void;
    crossTabReleaseStreamLock: () => void;
    crossTabDiagnostics: {
      sse: string[];
      sources: EventSource[];
      sent: OutgoingTabMessage[];
      received: (OutgoingTabMessage & { __receivedAt: number })[];
    };
    crossTabWorkspace: string;
    crossTabIdentity: string;
    crossTabSyncDb: Awaited<
      ReturnType<typeof import("../../src/sync/client.ts").syncDatabase>
    >["db"];
    crossTabCounts: CrossTabCounts;
    crossTabCountEvents: {
      resource: "state" | "transcript";
      version: ResourceVersion | undefined;
      received: number;
      sse: number;
      at: number;
    }[];
    crossTabHasStateWatch: boolean;
    crossTabStates: ResourceConnectionState[];
    crossTabStop: () => void;
    crossTabStopState: () => void;
    crossTabStopTranscript: () => void;
    crossTabStopConnection: () => void;
    crossTabStopLate: () => void;
    crossTabStopQueueCallbacks: () => void;
    crossTabStopCostCallbacks: () => void;
    crossTabStopLateQueue: () => void;
    crossTabStopTakeoverQueue: () => void;
    crossTabStopResumedQueue: () => void;
    crossTabLateCount: number;
    crossTabQueueCallbacks: number;
    crossTabCostCallbacks: number;
    crossTabLateQueueCallbacks: number;
    crossTabTakeoverQueueCallbacks: number;
    crossTabResumedQueueCount: number;
  }
}
