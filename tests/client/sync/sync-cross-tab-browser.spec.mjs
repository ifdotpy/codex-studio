// Same-origin Chromium proof: one elected sync stream fans out to eight tabs.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import { test, expect } from "../playwright.mjs";

test("sync-cross-tab-browser @performance", async ({
  browser: fixtureBrowser,
}) => {
  test.setTimeout(120_000);
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
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
      streamResources.set(res, resources);
      sendResourceEvent(res, currentWorkspace, "initial", resources);
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
  const pages = [];
  const waitFor = async (predicate, message, timeoutMs = 10000) => {
    const end = Date.now() + timeoutMs;
    while (Date.now() < end) {
      if (predicate()) return;
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
    await pages[1].evaluate(async () => {
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
    await waitFor(
      () => streams.size === 1 && streamsOpened > streamsBeforeHidingOwner,
      "a visible peer did not take ownership after the active owner was hidden",
      5000,
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
    const takeoverPanelBefore = await pages[1].evaluate(
      () => window.takeoverPanelCallbacks,
    );
    sendChange(workspaceId, {
      kind: "panel",
      agentId: "takeover-unique-panel",
    });
    await pages[1].waitForFunction(
      (previous) => window.takeoverPanelCallbacks > previous,
      takeoverPanelBefore,
      { timeout: 5000 },
    );
    const takeoverPanelAfterChange = await pages[1].evaluate(
      () => window.takeoverPanelCallbacks,
    );
    await pages[1].waitForTimeout(3500);
    assert.equal(
      await pages[1].evaluate(() => window.takeoverPanelCallbacks),
      takeoverPanelAfterChange,
      "quiet peer heartbeats do not refetch the surviving panel after takeover",
    );
    assert.ok(
      Date.now() - visibilityFailoverStarted <= 5000,
      "visible-peer ownership recovery exceeded five seconds",
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
    assert.equal(
      streams.size,
      1,
      "a resumed former owner cannot duplicate the current SSE",
    );
    const beforeOffline = await Promise.all(
      pages.map((page) => page.evaluate(() => ({ ...window.counts }))),
    );
    await context.setOffline(true);
    await waitFor(
      () => streams.size === 0,
      "the owner stream remained open after all tabs went offline",
    );
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
    const failoverStarted = Date.now();
    await pages[streamOwnerIndex].close();
    await waitFor(
      () => streams.size === 1 && streamsOpened >= 2,
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
    pages.splice(streamOwnerIndex, 1);
    const isolatedPage = await context.newPage();
    await isolatedPage.goto(
      `http://localhost:${server.httpServer.address().port}/sync-check`,
    );
    await isolatedPage.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      window.identity = (await client.syncDatabase()).workspaceId;
      window.stop = client.watchSyncInvalidations("state", () => {});
    });
    await waitFor(
      () => streams.size === 2,
      "a second workspace did not get its own elected stream",
    );
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
              window.counts.state > prior.state &&
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
      `PASS: 8 tabs, 1 active SSE, scoped transcript fan-out, failover ${failoverMs}ms\n`,
    );
  } finally {
    await noCoordination?.close();
    await context.close().catch(() => {});
    await server.close();
  }
});
