// Same-origin Chromium proof: one elected sync stream fans out to eight tabs.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import { test, expect, apiSchemaHandshakeSse } from "../playwright.mjs";

test("sync-cross-tab-browser @performance", async ({
  browser: fixtureBrowser,
}) => {
  test.setTimeout(120_000);
  const { createServer } = await import(
    new URL("../../node_modules/vite/dist/node/index.js", import.meta.url)
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  const workspaceId = "b".repeat(32);
  const otherWorkspaceId = "a".repeat(32);
  const workspaceFor = (req) =>
    req.headers.host?.startsWith("localhost:") ? otherWorkspaceId : workspaceId;
  const streams = new Set();
  let streamsOpened = 0;
  const streamWorkspaces = new Map();
  const streamResources = new Map();
  const revisions = new Map();
  let unchangedReconnectBaseline = false;
  const sendResourceEvent = (response, currentWorkspace, reason, resources) => {
    const revision = (revisions.get(currentWorkspace) || 0) + 1;
    revisions.set(currentWorkspace, revision);
    response.write(
      `event: resources\ndata: ${JSON.stringify({
        protocol: 3,
        workspaceId: currentWorkspace,
        epoch: "tab-epoch",
        revision,
        reason,
        resources,
      })}\n\n`,
    );
  };
  const sendSameVersionBaseline = (
    response,
    currentWorkspace,
    reason,
    resources,
  ) => {
    response.write(
      `event: resources\ndata: ${JSON.stringify({
        protocol: 3,
        workspaceId: currentWorkspace,
        epoch: "tab-epoch",
        revision: revisions.get(currentWorkspace) || 0,
        reason,
        resources,
      })}\n\n`,
    );
  };
  const sendSameVersionBaselineToStreams = (currentWorkspace) => {
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
  const sendChange = (currentWorkspace, resource) => {
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
  const apiFixture = (req, res, next) => {
    const path = new URL(req.url || "/", "http://localhost").pathname;
    if (path === "/sync-check") {
      res.setHeader("Content-Type", "text/html");
      res.end("<!doctype html><title>Cross tab sync</title>");
    } else if (path === "/api/sync/identity") {
      const currentWorkspace = workspaceFor(req);
      res.setHeader("Content-Type", "application/json");
      res.end(JSON.stringify({ workspaceId: currentWorkspace }));
    } else if (path === "/api/sync/pull") {
      const currentWorkspace = workspaceFor(req);
      res.setHeader("Content-Type", "application/json");
      res.end(
        JSON.stringify({
          workspaceId: currentWorkspace,
          documents: [],
          checkpoint: { seq: 0 },
        }),
      );
    } else if (path === "/api/sync/stream") {
      const currentWorkspace = workspaceFor(req);
      streamsOpened++;
      res.writeHead(200, {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        Connection: "keep-alive",
      });
      streams.add(res);
      streamWorkspaces.set(res, currentWorkspace);
      const resources = JSON.parse(
        new URL(req.url || "/", "http://localhost").searchParams.get(
          "resources",
        ) || "[]",
      );
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
  let noCoordination;
  let otherWorkspaceContext;
  const pages = [];
  const waitFor = async (predicate, message, timeoutMs = 10000) => {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      if (await predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 40));
    }
    assert.fail(message);
  };
  try {
    const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
    const streamLockName = `codex-sync-stream:${origin}:${workspaceId}`;
    const competingOwner = await context.newPage();
    await competingOwner.goto(`${origin}/sync-check`);
    await competingOwner.evaluate(async (name) => {
      if (!navigator.locks?.request)
        throw new Error("Web Locks are required for single-stream ownership");
      window.lockReady = new Promise((resolve) => {
        window.resolveLockReady = resolve;
      });
      void navigator.locks.request(name, async (lock) => {
        if (!lock)
          throw new Error("Failed to establish competing stream lease");
        window.resolveLockReady();
        await new Promise((resolve) => {
          window.releaseStreamLock = resolve;
        });
      });
      await window.lockReady;
    }, streamLockName);
    await context.addInitScript(() => {
      window.__syncDiagnostics = {
        sse: [],
        sources: [],
        sent: [],
        received: [],
      };
      const NativeSource = window.EventSource;
      window.EventSource = class extends NativeSource {
        constructor(...args) {
          super(...args);
          window.__syncDiagnostics.sources.push(this);
          this.addEventListener("message", (event) =>
            window.__syncDiagnostics.sse.push(event.data),
          );
          for (const name of ["resources", "heartbeat", "token-rates"])
            this.addEventListener(name, (event) =>
              window.__syncDiagnostics.sse.push(event.data),
            );
        }
      };
      const NativeChannel = window.BroadcastChannel;
      window.BroadcastChannel = class extends NativeChannel {
        constructor(...args) {
          super(...args);
          this.addEventListener("message", (event) =>
            window.__syncDiagnostics.received.push(event.data),
          );
        }
        postMessage(data) {
          window.__syncDiagnostics.sent.push(data);
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
        window.workspace = identity.workspaceId;
        window.syncDb = identity.db;
        window.counts = { state: 0, transcripts: 0 };
        window.hasStateWatch = true;
        window.stopState = client.watchResourceChanges(
          { kind: "state" },
          () => {
            window.counts.state++;
          },
        );
        window.stopTranscript = client.watchResourceChanges(
          { kind: "transcript", agentId: "session-one" },
          () => {
            window.counts.transcripts++;
          },
        );
        window.states = [];
        window.stopConnection = client.watchResourceConnection((state) =>
          window.states.push(state),
        );
      });
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
    assert.equal(
      streams.size,
      0,
      "no elected tab may open SSE while another owner holds the stream lease",
    );
    await competingOwner.evaluate(() => window.releaseStreamLock());
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
            window.workspace &&
            window.counts &&
            window.counts.state > 0 &&
            window.counts.transcripts > 0,
          null,
          { timeout: 5000 },
        ),
      ),
    );
    const beforeForeignSchemaMessage = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.counts }))),
    );
    await pages[7].evaluate(async (currentWorkspace) => {
      const { API_SCHEMA_HASH } = await import("/src/generated/apiSchema.ts");
      const channel = new BroadcastChannel(
        `codex-sync-${location.origin}-${currentWorkspace}`,
      );
      channel.postMessage({
        kind: "resource-event",
        workspaceId: currentWorkspace,
        tabId: "foreign-schema-tab",
        apiSchemaHash: `${API_SCHEMA_HASH}-stale`,
        event: {
          protocol: 3,
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
        pages.map((page) => page.evaluate(() => ({ ...window.counts }))),
      ),
      beforeForeignSchemaMessage,
      "Tabs ignore channel messages from a different schema hash",
    );
    await new Promise((resolve) => setTimeout(resolve, 1400));
    const before = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.counts }))),
    );
    const lateSubscriber = await context.newPage();
    await lateSubscriber.goto(`${origin}/sync-check`);
    await lateSubscriber.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.lateCount = 0;
      window.counts = { state: 0, transcripts: 0 };
      window.hasStateWatch = false;
      window.stopLate = client.watchResourceChanges(
        { kind: "transcript", agentId: "session-one" },
        () => {
          window.lateCount++;
          window.counts.transcripts++;
        },
      );
    });
    await lateSubscriber.waitForFunction(() => window.lateCount > 0);
    pages.push(lateSubscriber);
    before.push(await lateSubscriber.evaluate(() => ({ ...window.counts })));
    sendChange(workspaceId, {
      kind: "transcript",
      agentId: "session-one",
    });
    try {
      await Promise.all(
        pages.map((page, index) =>
          page.waitForFunction(
            (prior) =>
              window.counts.transcripts > prior.transcripts &&
              window.counts.state === prior.state,
            before[index],
            { timeout: 5000 },
          ),
        ),
      );
    } catch (error) {
      const diagnostics = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            counts: window.counts,
            sse: window.__syncDiagnostics.sse,
            sent: window.__syncDiagnostics.sent,
            received: window.__syncDiagnostics.received,
          })),
        ),
      );
      process.stderr.write(
        `fan-out diagnostic: ${JSON.stringify({ streamsOpened, diagnostics })}\n`,
      );
      throw error;
    }
    await pages[0].evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.panelCallbacks = 0;
      window.stopPanelCallbacks = client.watchResourceChanges(
        { kind: "panel", agentId: "stable-panel" },
        () => window.panelCallbacks++,
      );
    });
    await pages[2].evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.costCallbacks = 0;
      window.stopCostCallbacks = client.watchResourceChanges(
        { kind: "costs" },
        () => window.costCallbacks++,
      );
    });
    await Promise.all([
      pages[0].waitForFunction(() => window.panelCallbacks > 0),
      pages[2].waitForFunction(() => window.costCallbacks > 0),
    ]);
    const costBeforeUnrelated = await pages[2].evaluate(
      () => window.costCallbacks,
    );
    sendChange(workspaceId, { kind: "costs" });
    await pages[2].waitForFunction(
      (prior) => window.costCallbacks > prior,
      costBeforeUnrelated,
    );
    const panelFollower = pages[1];
    await panelFollower.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.latePanelCallbacks = 0;
      window.stopLatePanel = client.watchResourceChanges(
        { kind: "panel", agentId: "stable-panel" },
        () => window.latePanelCallbacks++,
      );
    });
    await panelFollower.waitForFunction(
      () => window.latePanelCallbacks > 0,
      undefined,
      { timeout: 5000 },
    );
    const latePanelBaseline = await panelFollower.evaluate(
      () => window.latePanelCallbacks,
    );
    const latePanelAfterUnrelated = await panelFollower.evaluate(
      () => window.latePanelCallbacks,
    );
    assert.equal(
      latePanelAfterUnrelated,
      latePanelBaseline,
      "An unrelated resource event does not change the panel callback version",
    );
    sendChange(workspaceId, { kind: "panel", agentId: "stable-panel" });
    await panelFollower.waitForFunction(
      (prior) => window.latePanelCallbacks > prior,
      latePanelAfterUnrelated,
      { timeout: 5000 },
    );
    const stablePanelCallbacks = await panelFollower.evaluate(
      () => window.latePanelCallbacks,
    );
    const costBeforeHeartbeat = await pages[2].evaluate(
      () => window.costCallbacks,
    );
    sendChange(workspaceId, { kind: "costs" });
    await pages[2].waitForFunction(
      (prior) => window.costCallbacks > prior,
      costBeforeHeartbeat,
    );
    await panelFollower.waitForTimeout(3500);
    assert.equal(
      await panelFollower.evaluate(() => window.latePanelCallbacks),
      stablePanelCallbacks,
      "Peer subscription heartbeats do not replay unchanged resources after unrelated traffic",
    );
    const initialOwnerIndex = await Promise.any(
      pages.map(async (page, index) =>
        (await page.evaluate(() =>
          window.__syncDiagnostics.sources.some(
            (source) => source.readyState === EventSource.OPEN,
          ),
        ))
          ? index
          : Promise.reject(),
      ),
    );
    const takeoverSubscriberIndex = initialOwnerIndex === 1 ? 0 : 1;
    await pages[takeoverSubscriberIndex].evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.takeoverPanelCallbacks = 0;
      window.stopTakeoverPanel = client.watchResourceChanges(
        { kind: "panel", agentId: "takeover-unique-panel" },
        () => window.takeoverPanelCallbacks++,
      );
    });
    await waitFor(
      () =>
        [...streamResources.values()].some((resources) =>
          resources.some(
            (resource) =>
              resource.kind === "panel" &&
              resource.agentId === "takeover-unique-panel",
          ),
        ),
      "the original owner did not learn the second tab's unique panel subscription",
    );
    await pages[0].evaluate(() => window.stopPanelCallbacks());
    await pages[2].evaluate(() => window.stopCostCallbacks());
    await panelFollower.evaluate(() => window.stopLatePanel());
    const streamsBeforeHidingOwner = streamsOpened;
    await pages[initialOwnerIndex].evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: true,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    try {
      await pages[initialOwnerIndex].waitForFunction(
        () =>
          window.__syncDiagnostics.sources.every(
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
            states: window.states,
            sources: window.__syncDiagnostics.sources.map((source) => ({
              readyState: source.readyState,
              url: source.url,
            })),
            sent: window.__syncDiagnostics.sent,
            received: window.__syncDiagnostics.received,
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
                    window.__syncDiagnostics.sources.some(
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
            states: window.states,
            counts: window.counts,
            sent: window.__syncDiagnostics.sent.slice(-12),
            received: window.__syncDiagnostics.received.slice(-12),
            sources: window.__syncDiagnostics.sources.map((source) => ({
              readyState: source.readyState,
              url: source.url,
            })),
          })),
        ),
      );
      throw new Error(
        `${error.message}; ${JSON.stringify({
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
        })}`,
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
            window.__syncDiagnostics.sources.filter(
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
    const takeoverConnectionCount = streamsOpened - streamsBeforeHidingOwner;
    assert.equal(
      takeoverConnectionCount,
      1,
      "takeover opens exactly one replacement stream request",
    );
    process.stdout.write(
      `PASS ownership accounting: ${streamsBeforeHidingOwner} streams before hide, ${takeoverConnectionCount} requests during takeover, ${streamsOpened} total\n`,
    );
    await waitFor(
      () =>
        [...streamResources.values()].some((resources) =>
          resources.some(
            (resource) =>
              resource.kind === "panel" &&
              resource.agentId === "takeover-unique-panel",
          ),
        ),
      "the replacement owner did not rediscover the surviving peer's unique panel",
    );
    const takeoverPanelBefore = await pages[takeoverSubscriberIndex].evaluate(
      () => window.takeoverPanelCallbacks,
    );
    sendChange(workspaceId, {
      kind: "panel",
      agentId: "takeover-unique-panel",
    });
    await pages[takeoverSubscriberIndex].waitForFunction(
      (previous) => window.takeoverPanelCallbacks > previous,
      takeoverPanelBefore,
      { timeout: 5000 },
    );
    const takeoverPanelAfterChange = await pages[
      takeoverSubscriberIndex
    ].evaluate(() => window.takeoverPanelCallbacks);
    await pages[takeoverSubscriberIndex].waitForTimeout(3500);
    assert.equal(
      await pages[takeoverSubscriberIndex].evaluate(
        () => window.takeoverPanelCallbacks,
      ),
      takeoverPanelAfterChange,
      "quiet peer heartbeats do not refetch the surviving panel after takeover",
    );
    const resumedOwnerIndex = await Promise.any(
      pages.map(async (page, index) =>
        (await page.evaluate(() =>
          window.__syncDiagnostics.sources.some(
            (source) => source.readyState === EventSource.OPEN,
          ),
        ))
          ? index
          : Promise.reject(),
      ),
    );
    const followerIndex = pages.findIndex(
      (_, index) => index !== resumedOwnerIndex && index !== initialOwnerIndex,
    );
    const follower = pages[followerIndex];
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
      window.resumedPanelCount = 0;
      window.stopResumedPanel = client.watchResourceChanges(
        { kind: "panel", agentId: "resumed-follower" },
        () => window.resumedPanelCount++,
      );
    });
    await waitFor(
      () =>
        [...streamResources.values()].some((resources) =>
          resources.some(
            (resource) =>
              resource.kind === "panel" &&
              resource.agentId === "resumed-follower",
          ),
        ),
      "the resumed follower's new ref was not added to the owner's stream",
    );
    await follower.waitForFunction(
      () => window.resumedPanelCount > 0,
      undefined,
      { timeout: 5000 },
    );
    await new Promise((resolve) => setTimeout(resolve, 11_000));
    assert.ok(
      [...streamResources.values()].some((resources) =>
        resources.some(
          (resource) =>
            resource.kind === "panel" &&
            resource.agentId === "resumed-follower",
        ),
      ),
      "the resumed follower kept its ref advertised past peer expiry",
    );
    const resumedBefore = await follower.evaluate(
      () => window.resumedPanelCount,
    );
    sendChange(workspaceId, {
      kind: "panel",
      agentId: "resumed-follower",
    });
    await follower.waitForFunction(
      (previous) => window.resumedPanelCount > previous,
      resumedBefore,
      { timeout: 5000 },
    );
    await follower.evaluate(() => window.stopResumedPanel());
    await pages[1].evaluate(() => window.stopTakeoverPanel());
    await pages[initialOwnerIndex].evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: false,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await new Promise((resolve) => setTimeout(resolve, 100));
    await waitFor(
      () => streams.size === 1,
      "a resumed former owner left a duplicate stream after close propagation",
      1500,
    );
    const resumedOwnerDiagnostic = await Promise.all(
      pages.map((page, index) =>
        page.evaluate(
          (index) => ({
            index,
            hidden: document.hidden,
            states: window.states,
            sources: window.__syncDiagnostics.sources.map((source) => ({
              readyState: source.readyState,
              url: source.url,
            })),
            received: window.__syncDiagnostics.received.slice(-6),
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
    const beforeOffline = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.counts }))),
    );
    const hadStateWatch = await Promise.all(
      pages.map((page) => page.evaluate(() => window.hasStateWatch)),
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
              (!needsState || window.counts.state > prior.state) &&
              window.counts.transcripts > prior.transcripts,
            { prior: beforeOffline[index], needsState: index < 8 },
            { timeout: 5000 },
          ),
        ),
      );
    } catch (error) {
      const diagnostics = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            hidden: document.hidden,
            counts: window.counts,
            states: window.states,
            sse: window.__syncDiagnostics.sse.slice(-5),
            received: window.__syncDiagnostics.received.slice(-8),
          })),
        ),
      );
      process.stderr.write(
        `resume baseline diagnostic: ${JSON.stringify({ beforeOffline, streamsOpened, diagnostics })}\n`,
      );
      throw error;
    }
    const afterRecovery = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.counts }))),
    );
    for (let index = 0; index < pages.length; index++) {
      assert.deepEqual(
        afterRecovery[index],
        {
          state: beforeOffline[index].state + Number(hadStateWatch[index]),
          transcripts: beforeOffline[index].transcripts + 1,
        },
        `tab ${index} reconciles its unchanged same-version resume baseline exactly once`,
      );
    }
    sendSameVersionBaselineToStreams(workspaceId);
    await pages[0].waitForTimeout(3500);
    const afterDuplicateAndHeartbeats = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.counts }))),
    );
    assert.deepEqual(
      afterDuplicateAndHeartbeats,
      afterRecovery,
      "duplicate unchanged baselines and peer heartbeats do not refetch resources",
    );
    const streamOwnerIndex = await Promise.any(
      pages.map(async (page, index) =>
        (await page.evaluate(() =>
          window.__syncDiagnostics.sources.some(
            (source) => source.readyState === EventSource.OPEN,
          ),
        ))
          ? index
          : Promise.reject(),
      ),
    );
    const replacementFollowerIndex = streamOwnerIndex === 0 ? 1 : 0;
    await pages[replacementFollowerIndex].evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: false,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.equal(streams.size, 1, "one owner stream remains before failover");
    assert.equal(
      await pages[streamOwnerIndex].evaluate(
        async (name) =>
          (await navigator.locks.query()).held.some(
            (lock) => lock.name === name,
          ),
        streamLockName,
      ),
      true,
      "the selected stream owner still holds its workspace lock immediately before close",
    );
    const failoverStarted = Date.now();
    const streamsOpenedBeforeFailover = streamsOpened;
    await pages[streamOwnerIndex].close();
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
            window.__syncDiagnostics.sources.some(
              (source) => source.readyState === EventSource.OPEN,
            ),
          undefined,
          { timeout: 5000 },
        );
        return index;
      }),
    );
    await pages[workspaceOwnerIndex].evaluate(() => {
      Object.defineProperty(document, "hidden", {
        configurable: true,
        value: false,
      });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    otherWorkspaceContext = await browser.newContext();
    const isolatedPage = await otherWorkspaceContext.newPage();
    await isolatedPage.goto(
      `http://localhost:${server.httpServer.address().port}/sync-check`,
    );
    await isolatedPage.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.identity = (await client.syncDatabase()).workspaceId;
      window.stop = client.watchSyncInvalidations("state", () => {});
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
              workspace: window.workspace,
              identity: window.identity,
              states: window.states,
              sources: (window.__syncDiagnostics?.sources || []).map(
                (source) => ({
                  readyState: source.readyState,
                  url: source.url,
                }),
              ),
              received: window.__syncDiagnostics?.received.slice(-5) || [],
            }),
            index,
          ),
        ),
      );
      throw new Error(
        `${error.message}; workspace stream diagnostic: ${JSON.stringify({ streamsOpened, activeWorkspaces: [...streams].map((response) => streamWorkspaces.get(response)), diagnostics })}`,
      );
    }
    assert.equal(
      await isolatedPage.evaluate(() => window.identity),
      otherWorkspaceId,
    );
    const beforeWorkspaceMove = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.counts }))),
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
              (!window.hasStateWatch || window.counts.state > prior.state) &&
              window.counts.transcripts > prior.transcripts,
            beforeWorkspaceMove[index],
            { timeout: 4500 },
          ),
        ),
      );
    } catch (error) {
      const diagnostics = await Promise.all(
        pages.map((page) =>
          page.evaluate(() => ({
            counts: window.counts,
            states: window.states,
            sse: window.__syncDiagnostics.sse.slice(-8),
            sent: window.__syncDiagnostics.sent.slice(-5),
            received: window.__syncDiagnostics.received.slice(-8),
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
    expect(streams.size).toBe(0);
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
      window.stop = client.watchResourceChanges({ kind: "state" }, () => {});
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
});
