import type { LegacyPullFixture } from "./prefetch-storage.fixture.ts";
import {
  projectionIsStored,
  completedTranscriptAgent,
} from "./prefetch-storage.fixture.ts";
import type { GetResult, PostBody, PostResult } from "../../src/apiTypes.ts";
import type { Page, Route } from "@playwright/test";
import type { components } from "../../src/generated/api.ts";
import type { Agent } from "../../src/types.ts";
import { fixtureGetResult, fixturePostBody } from "./http.ts";
import { initializeInstrumentation } from "./chat-switch-latency-ui.fixture.ts";
import {
  readTestState,
  stubEntityState,
  readFixtureSyncContract,
  readApiSchemaHash,
  entityPullFixture,
  test,
  spawnFixture as spawn,
  rustUiFixture,
} from "../playwright.ts";
// Real renderer and isolated server. Delayed streams must not delay navigation.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
test(
  "Chat switch latency ui",
  { tag: "@performance" },
  async ({
    browser: testBrowser,
    page: runnerPage,
    context: runnerContext,
  }) => {
    test.setTimeout(300_000);
    const dir = await mkdtemp(join(tmpdir(), "studio-chat-switch-"));
    const proc = spawn(process.execPath, [rustUiFixture, dir], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    let log = "",
      page: Page | undefined;
    proc.stderr?.on("data", (d) => (log += d));
    try {
      const port = await new Promise<number>((resolve, reject) => {
        proc.stdout?.once("data", (d) => resolve(Number(String(d).trim())));
        proc.once("exit", () => reject(Error(log)));
      });
      const origin = `http://127.0.0.1:${port}`;
      const state = await readTestState(origin);
      const a = state.threads.find((x) => x.name === "Other project");
      const b = state.threads.find((x) => x.name === "Release lead");
      assert.ok(a && b, "Expected both fixture chats");
      // Background transcript warming keeps only the top-ranked desktop chat.
      // Pin one other managed chat so this cold target stays outside that watch.
      const initialTopWatchRows = state.threads
        .filter(
          (agent) =>
            agent.id !== a.id &&
            agent.id !== b.id &&
            agent.source === "managed" &&
            !agent.archived,
        )
        .slice(0, 1);
      assert.equal(initialTopWatchRows.length, 1);
      for (const agent of initialTopWatchRows) agent.pinned = true;
      Object.assign(a, { pinned: false, updated: 0 });
      const activePage = runnerPage;
      page = activePage;
      await activePage.setViewportSize({ width: 1440, height: 960 });
      activePage.setDefaultTimeout(10000);
      const errors: string[] = [];
      activePage.on("pageerror", (e) => errors.push(e.message));
      await activePage.addInitScript(initializeInstrumentation);
      const snapshot = {
        ...state,
        threads: state.threads.map((x) => ({
          ...x,
          status: "completed",
          inFlight: false,
          turnId: null,
        })),
        runtime: { ...state.runtime, requests: [] },
      };
      const syncContract = await readFixtureSyncContract(origin);
      const entitySync = await stubEntityState(page, snapshot, syncContract);
      const payload = (
        agent: Agent,
        tag: string,
      ): GetResult<"/api/transcript/page"> &
        Partial<
          Pick<components["schemas"]["TranscriptStreamDelta"], "order">
        > => ({
        agent: completedTranscriptAgent(agent),
        truncated: false,
        items: Array.from({ length: 30 }, (_, i) => ({
          id: `${tag}-${i}`,
          role: "assistant",
          text: `History ${tag} ${i}. ${"Visible saved text. ".repeat(15)}`,
        })),
      });
      const requests: { id: string; at: number; route: Route }[] = [];
      const visited = new Set();
      let hold = false;
      let coldSwitchArmed = false;
      let resolveNextA: ((route: Route) => void) | undefined;
      let resolveHeldB: ((route: Route) => void) | undefined;
      let latestA = payload(a, "a");
      let latestASequence = 10;
      const responseFor = (
        id: string,
        data: GetResult<"/api/transcript/page">,
        seq: number,
        after = 0,
      ) => ({
        ...syncContract.identity,
        ...entityPullFixture(snapshot, {
          scope: `transcript:${id}`,
          after,
          transcript: data,
          transcriptSeq: seq,
        }),
      });
      await activePage.route("**/api/sync/pull?*", async (r) => {
        const url = new URL(r.request().url());
        const scope = url.searchParams.get("scope");
        if (!scope?.startsWith("transcript:")) return r.fallback();
        const id = scope.slice("transcript:".length);
        requests.push({ id, at: Date.now(), route: r });
        if (id === a.id && resolveNextA) {
          const resolve = resolveNextA;
          resolveNextA = undefined;
          resolve(r);
          return;
        }
        if (hold && id === b.id && resolveHeldB) {
          resolveHeldB(r);
          resolveHeldB = undefined;
          return;
        }
        if (id === a.id && !coldSwitchArmed) {
          await r.fulfill({
            json: responseFor(a.id, latestA, latestASequence, latestASequence),
          });
          return;
        }
        if (!hold || id === a.id)
          await r.fulfill({
            json: responseFor(
              id,
              id === a.id ? latestA : payload(b, "b"),
              id === a.id ? latestASequence : 20,
              Number(url.searchParams.get("after") || 0),
            ),
          });
      });
      await activePage.goto(origin);
      await activePage.waitForFunction(() =>
        performance
          .getEntriesByType("resource")
          .filter(
            (entry): entry is PerformanceResourceTiming =>
              entry instanceof PerformanceResourceTiming,
          )
          .some((entry) => {
            const url = new URL(entry.name);
            return url.pathname === "/api/session" && entry.responseEnd > 0;
          }),
      );
      await activePage.locator("#conversation-title").waitFor();
      await activePage.locator(`[data-chat="${b.id}"]`).waitFor();
      await activePage.locator(`[data-chat="${b.id}"]`).click();
      await page
        .locator("#conversation-title")
        .getByText("Release lead", { exact: true })
        .waitFor();
      await activePage
        .locator("#messages [data-message^='b-']")
        .first()
        .waitFor();
      console.log(
        "chat-switch startup readiness",
        JSON.stringify(
          await activePage.evaluate(() => {
            const resources = performance
              .getEntriesByType("resource")
              .filter(
                (entry): entry is PerformanceResourceTiming =>
                  entry instanceof PerformanceResourceTiming,
              )
              .filter((entry) =>
                new URL(entry.name).pathname.startsWith("/api/"),
              );
            const identity = resources.find(
              (entry) => new URL(entry.name).pathname === "/api/sync/identity",
            );
            const session = resources.find(
              (entry) => new URL(entry.name).pathname === "/api/session",
            );
            return {
              navigationStart: 0,
              firstSidebarRowAt: window.switchLatencyFirstSidebarRowVisibleAt,
              sessionRequestStartAt: session?.startTime ?? null,
              sessionResponseAt: session?.responseEnd ?? null,
              identityRequestStartAt: identity?.startTime ?? null,
              identityResponseAt: identity?.responseEnd ?? null,
              dbOpenAfterIdentityMs:
                identity && session
                  ? session.startTime - identity.responseEnd
                  : null,
              initialTitle: document.querySelector("#conversation-title")
                ?.textContent,
              initialTranscriptRows: document.querySelectorAll(
                "#messages [data-message]",
              ).length,
              requests: resources.map((entry) => ({
                path: new URL(entry.name).pathname,
                startAt: entry.startTime,
                durationMs: entry.duration,
              })),
            };
          }),
        ),
      );
      const open = async (id: string, message: string) => {
        const firstVisit = !visited.has(id);
        const chat = activePage.locator(`[data-chat="${id}"]`);
        const selector = `[data-message="${message}"]`;
        const prefix = message.slice(0, message.lastIndexOf("-"));
        await chat.evaluate((element, target) => {
          window.switchLatencyPrepareStartedAt = undefined;
          window.switchLatencyStartedAt = undefined;
          window.switchLatencyFirstRowsPaintAt = undefined;
          window.switchLatencyPaintAt = undefined;
          window.switchLatencyScrollStartedAt = undefined;
          const markPrepareIntent = (event: Event) => {
            if (
              event.target instanceof Element &&
              event.target.closest(`[data-chat="${element.dataset.chat}"]`)
            ) {
              window.switchLatencyPrepareStartedAt = performance.now();
              window.removeEventListener(
                "pointerover",
                markPrepareIntent,
                true,
              );
            }
          };
          window.addEventListener("pointerover", markPrepareIntent, true);
          element.addEventListener(
            "pointerdown",
            () => {
              window.switchLatencyStartedAt = performance.now();
              const messages = document.querySelector("#messages");
              const messageBounds = messages?.getBoundingClientRect();
              const targetPrefix = target;
              const recordFirstRows = () => {
                const row = Array.from(
                  document.querySelectorAll("#messages [data-message]"),
                ).find((node) => {
                  if (
                    !node
                      .getAttribute("data-message")
                      ?.startsWith(`${targetPrefix}-`)
                  )
                    return false;
                  const bounds = node.getBoundingClientRect();
                  return (
                    bounds.bottom > (messageBounds?.top || 0) &&
                    bounds.top < (messageBounds?.bottom || 0)
                  );
                });
                if (row?.isConnected) {
                  window.switchLatencyFirstRowsPaintAt = performance.now();
                  return;
                }
                requestAnimationFrame(recordFirstRows);
              };
              const recordVisibleTarget = () => {
                const node = document.querySelector(target);
                const bounds = node?.getBoundingClientRect();
                const viewport = document
                  .querySelector("#messages")
                  ?.getBoundingClientRect();
                if (
                  node?.isConnected &&
                  bounds &&
                  viewport &&
                  bounds.bottom > viewport.top &&
                  bounds.top < viewport.bottom
                ) {
                  window.switchLatencyPaintAt = performance.now();
                  return;
                }
                requestAnimationFrame(recordVisibleTarget);
              };
              requestAnimationFrame(recordFirstRows);
              requestAnimationFrame(recordVisibleTarget);
            },
            { once: true },
          );
        }, prefix);
        if (id === a.id && firstVisit) coldSwitchArmed = true;
        await chat.click();
        await activePage.waitForFunction(
          () => typeof window.switchLatencyFirstRowsPaintAt === "number",
        );
        const firstRowsTiming = await activePage.evaluate((id) => {
          const startedAt = window.switchLatencyStartedAt;
          const prepareStartedAt = window.switchLatencyPrepareStartedAt;
          if (startedAt === undefined)
            throw new Error("Missing chat switch gesture");
          const gestureStartedAt = prepareStartedAt ?? startedAt;
          const firstRowsAt = window.switchLatencyFirstRowsPaintAt;
          if (firstRowsAt === undefined)
            throw new Error("Missing first rows timestamp");
          const resource = performance
            .getEntriesByType("resource")
            .filter(
              (entry): entry is PerformanceResourceTiming =>
                entry instanceof PerformanceResourceTiming,
            )
            .filter((entry) => {
              const url = new URL(entry.name);
              return (
                url.pathname === "/api/sync/pull" &&
                url.searchParams.get("scope") === `transcript:${id}` &&
                entry.startTime >= gestureStartedAt &&
                entry.startTime <= firstRowsAt
              );
            })
            .sort((left, right) => left.startTime - right.startTime)[0];
          return {
            clickToFirstRowsMs: firstRowsAt - startedAt,
            prepareToFirstRowsMs:
              typeof prepareStartedAt === "number"
                ? firstRowsAt - prepareStartedAt
                : null,
            prepareToPullStartMs:
              resource && typeof prepareStartedAt === "number"
                ? resource.startTime - prepareStartedAt
                : null,
            fixturePullStartAfterGestureMs: resource
              ? resource.startTime - gestureStartedAt
              : null,
            fixturePullStartAfterClickMs: resource
              ? resource.startTime - startedAt
              : null,
            fixturePullResponseMs: resource?.duration ?? null,
            firstRowsAfterPullResponseMs: resource
              ? Math.max(0, firstRowsAt - resource.responseEnd)
              : null,
            pullStillPendingAtFirstRowsMs: resource
              ? Math.max(0, resource.responseEnd - firstRowsAt)
              : null,
            precedingRequests: performance
              .getEntriesByType("resource")
              .filter(
                (entry): entry is PerformanceResourceTiming =>
                  entry instanceof PerformanceResourceTiming,
              )
              .filter((entry) => {
                const url = new URL(entry.name);
                return (
                  url.pathname.startsWith("/api/") &&
                  entry.startTime >= gestureStartedAt &&
                  entry.startTime <= firstRowsAt
                );
              })
              .map((entry) => ({
                path: new URL(entry.name).pathname,
                startAfterClickMs: entry.startTime - startedAt,
                durationMs: entry.duration,
              })),
            longTasks: window.switchLatencyLongTasks
              .filter(
                (entry) =>
                  entry.startTime >= gestureStartedAt &&
                  entry.startTime <= firstRowsAt,
              )
              .map((entry) => ({
                startAfterClickMs: entry.startTime - startedAt,
                durationMs: entry.duration,
              })),
          };
        }, id);
        if (!visited.has(id)) {
          await activePage.locator("#messages").evaluate((messages) => {
            window.switchLatencyScrollStartedAt = performance.now();
            messages.scrollTop = messages.scrollHeight;
            messages.dispatchEvent(new Event("scroll"));
          });
          visited.add(id);
        }
        await activePage.locator(selector).waitFor({ state: "visible" });
        const targetMountAfterScrollMs = await activePage.evaluate(() =>
          typeof window.switchLatencyScrollStartedAt === "number"
            ? performance.now() - window.switchLatencyScrollStartedAt
            : null,
        );
        const timing = {
          id,
          ...firstRowsTiming,
          scrollAndTargetMountMs: targetMountAfterScrollMs,
        };
        console.log("chat-switch timing", JSON.stringify(timing));
        assert.equal(
          typeof timing.prepareToFirstRowsMs,
          "number",
          "Pointer entry is recorded before opening the row",
        );
        if (id === a.id && firstVisit) {
          assert.ok(
            timing.prepareToPullStartMs !== null &&
              timing.prepareToPullStartMs >= 0,
            `The cold transcript pull starts at or after the row prepare gesture: ${JSON.stringify(timing)}`,
          );
        }
        return firstRowsTiming.clickToFirstRowsMs;
      };
      await page
        .locator("#conversation-title")
        .getByText("Release lead", { exact: true })
        .waitFor();
      await activePage
        .locator("#messages [data-message^='b-']")
        .first()
        .waitFor();
      await activePage.waitForFunction(() => {
        const entries = performance
          .getEntriesByType("resource")
          .filter(
            (entry): entry is PerformanceResourceTiming =>
              entry instanceof PerformanceResourceTiming,
          );
        return entries.some((entry) => {
          const url = new URL(entry.name);
          return url.pathname === "/api/session" && entry.responseEnd > 0;
        });
      });
      const projectionDatabaseName =
        `rxdb-dexie-studio-entity-projection-${syncContract.identity.workspaceId}-` +
        `${readApiSchemaHash()}--0--projections`;
      const targetWasCold = await activePage.evaluate(projectionIsStored, {
        databaseName: projectionDatabaseName,
        documentId: `transcript:${a.id}`,
        onlyIfExists: true,
      });
      assert.equal(
        targetWasCold,
        false,
        "The target chat has no stored transcript before the user opens it",
      );
      await activePage.waitForFunction(() =>
        window.switchLatencyResourceFrames.some(
          (frame) => frame.reason === "initial",
        ),
      );
      const targetInInitialSubscription = await activePage.evaluate(
        (targetId) =>
          window.switchLatencyResourceFrames.some(
            (frame) =>
              frame.reason === "initial" &&
              frame.resources?.some(
                (resource) =>
                  resource.kind === "transcript" &&
                  resource.agentId === targetId,
              ),
          ),
        a.id,
      );
      assert.equal(
        targetInInitialSubscription,
        false,
        "The cold target is outside the single background transcript subscription",
      );
      const priorTargetPulls = requests.filter((item) => item.id === a.id);
      assert.equal(
        priorTargetPulls.length,
        0,
        `The cold chat has no transcript pull before pointer intent; pulls=${JSON.stringify(priorTargetPulls.map(({ at, route }) => ({ at, scope: new URL(route.request().url()).searchParams.get("scope") })))}`,
      );
      const pullsBeforeColdOpen = requests.filter(
        (item) => item.id === a.id,
      ).length;
      const cold = await open(a.id, "a-29");
      const coldOpenPulls = requests
        .filter((item) => item.id === a.id)
        .slice(pullsBeforeColdOpen);
      assert.ok(
        coldOpenPulls.length > 0,
        "Opening the cold chat requests its transcript from entity sync",
      );
      console.log(
        "cold-open transcript requests",
        JSON.stringify(
          coldOpenPulls.map(({ id, at, route }) => ({
            id,
            path: new URL(route.request().url()).pathname,
            scope: new URL(route.request().url()).searchParams.get("scope"),
            at,
          })),
        ),
      );
      console.log(
        "cold-open prepare trace",
        JSON.stringify(
          await activePage.evaluate(
            (targetId) => ({
              targetedFrames: window.switchLatencyResourceFrames
                .filter((frame) =>
                  frame.resources?.some(
                    (resource) =>
                      resource.kind === "transcript" &&
                      resource.agentId === targetId,
                  ),
                )
                .map(({ reason, receivedAt, resources }) => ({
                  reason,
                  receivedAt,
                  transcriptAgentIds: resources
                    .filter((resource) => resource.kind === "transcript")
                    .map((resource) => resource.agentId),
                })),
            }),
            a.id,
          ),
        ),
      );
      assert.ok(
        cold < 1500,
        `Cold history appears without the old three-second delay: ${cold}ms`,
      );
      await activePage.locator("#messages").evaluate((e) => {
        e.scrollTop = 950;
        e.dispatchEvent(new Event("scroll"));
      });
      const savedTop = await page
        .locator("#messages")
        .evaluate((e) => e.scrollTop);
      await open(b.id, "b-29");
      hold = true;
      const previousReads = requests.filter((x) => x.id === a.id).length;
      const refresh = new Promise<Route>((resolve) => (resolveNextA = resolve));
      const cached = await open(a.id, "a-29");
      assert.ok(
        cached < 500,
        `Saved history appears before a blocked refresh: ${cached}ms`,
      );
      assert.equal(
        await activePage.locator('[data-message="b-29"]').count(),
        0,
        "No other chat history leaks",
      );
      assert.ok(
        Math.abs(
          (await activePage.locator("#messages").evaluate((e) => e.scrollTop)) -
            savedTop,
        ) < 2,
        "Saved scroll position survives instant history restore",
      );
      const pending = await refresh;
      assert.ok(
        requests.filter((x) => x.id === a.id).length > previousReads,
        "Revisit still refreshes",
      );
      const fresh = payload(a, "fresh");
      fresh.order = fresh.items.map((x) => x.id);
      latestA = fresh;
      latestASequence = 30;
      const freshPull = activePage.waitForResponse((response) => {
        const url = new URL(response.url());
        return (
          url.pathname === "/api/sync/pull" &&
          url.searchParams.get("scope") === `transcript:${a.id}` &&
          Number(url.searchParams.get("after") || 0) < latestASequence
        );
      });
      await entitySync.update(snapshot, {
        origin,
        token: state.token,
        resources: [{ kind: "transcript", agentId: a.id }],
      });
      await pending.fulfill({
        json: responseFor(
          a.id,
          payload(a, "obsolete"),
          20,
          Number(
            new URL(pending.request().url()).searchParams.get("after") || 0,
          ),
        ),
      });
      await freshPull;
      await activePage.locator("#messages").evaluate((messages) => {
        messages.scrollTop = messages.scrollHeight;
        messages.dispatchEvent(new Event("scroll"));
      });
      await activePage.locator('[data-message="fresh-29"]').waitFor();
      await activePage.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
      assert.equal(
        await activePage.locator('[data-message="obsolete-29"]').count(),
        0,
        "Delayed HTTP cannot replace a newer stream",
      );
      const heldB = new Promise<Route>((resolve) => (resolveHeldB = resolve));
      const lateRequest = activePage.waitForRequest((request) => {
        const url = new URL(request.url());
        return (
          url.pathname === "/api/sync/pull" &&
          url.searchParams.get("scope") === `transcript:${b.id}`
        );
      });
      await open(b.id, "b-29");
      await lateRequest;
      const lateOther = await heldB;
      const refreshed = await open(a.id, "fresh-29");
      assert.ok(refreshed < 500, "Cache retains the newest stream");
      const lateOtherUrl = new URL(lateOther.request().url());
      await lateOther.fulfill({
        json: responseFor(
          b.id,
          payload(b, "late-other"),
          30,
          Number(lateOtherUrl.searchParams.get("after") || 0),
        ),
      });
      await activePage.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
      assert.equal(
        await activePage.locator('[data-message="late-other-29"]').count(),
        0,
        "Response from a previous chat cannot replace current history",
      );
      // Exercise the real RxDB projection path, including a slow pull on revisit.
      await activePage.unroute("**/api/sync/**");
      const identity = await fixtureGetResult(
        "/api/sync/identity",
        await fetch(origin + "/api/sync/identity"),
      );
      let projectionDelay = 0;
      let syncPulls = 0;
      const waitForProjection = async (id: string, sequence: number) => {
        const databaseName = `rxdb-dexie-studio-entity-projection-${identity.workspaceId}-${readApiSchemaHash()}--0--projections`;
        await activePage.waitForFunction(projectionIsStored, {
          databaseName,
          documentId: `transcript:${id}`,
          sequence,
        });
      };
      await activePage.route("**/api/sync/pull?*", async (r) => {
        const url = new URL(r.request().url());
        const scope = url.searchParams.get("scope");
        if (!scope?.startsWith("transcript:")) return r.fallback();
        syncPulls++;
        if (projectionDelay)
          await new Promise((resolve) => setTimeout(resolve, projectionDelay));
        const selected = scope.slice("transcript:".length) === a.id ? a : b;
        const data = payload(
          selected,
          selected.id === a.id ? "sync-a" : "sync-b",
        );
        assert.ok(data.agent);
        const runningAgent = {
          ...data.agent,
          status: "running",
          inFlight: true,
          turnId: "fixture-live-turn",
        };
        data.agent = runningAgent;
        const seq = selected.id === a.id ? 100 : 101;
        const after = Number(url.searchParams.get("after") || 0);
        await r.fulfill({
          json: {
            ...identity,
            documents:
              after < seq
                ? [
                    {
                      id: scope,
                      payload: JSON.stringify(data),
                      seq,
                      _deleted: false,
                    },
                  ]
                : [],
            checkpoint: { seq: Math.max(after, seq) },
          } satisfies LegacyPullFixture,
        });
      });
      // Preserve the old server's byte comparison in this fixture. New UI writes
      // must clear drafts safely before a busy backend can be upgraded.
      let legacyDraftConflicts = 0;
      await activePage.route("**/api/sync/drafts", async (r) => {
        const old = await fixtureGetResult(
          "/api/sync/pull",
          await fetch(origin + "/api/sync/pull?scope=drafts"),
        );
        if (!("documents" in old)) throw new Error("Unexpected drafts reset");
        const conflicts = [];
        for (const row of fixturePostBody(
          "/api/sync/drafts",
          r.request().postDataJSON(),
        ).rows) {
          const next = row.newDocumentState;
          const previous = old.documents.find(
            (x) => x.id === next.id && "payload" in x,
          );
          const assumed = row.assumedMasterState;
          if (
            previous &&
            "payload" in previous &&
            (!assumed ||
              assumed.payload !== previous.payload ||
              ("_deleted" in assumed && !!assumed._deleted) !==
                !!previous._deleted) &&
            (next.payload !== previous.payload ||
              ("_deleted" in next && !!next._deleted) !== !!previous._deleted)
          )
            conflicts.push(previous);
        }
        if (conflicts.length) {
          legacyDraftConflicts += conflicts.length;
          return r.fulfill({ json: conflicts });
        }
        return r.fallback();
      });
      hold = false;
      await activePage.reload();
      await open(a.id, "sync-a-29");
      projectionDelay = 1000;
      await activePage.locator(`[data-chat="${b.id}"]`).click();
      await activePage.locator('[data-message="sync-b-29"]').waitFor();
      await waitForProjection(a.id, 100);
      await waitForProjection(b.id, 101);
      const pullsBeforeQuickReturn = syncPulls;
      const quickProjectionReturn = await open(a.id, "sync-a-29");
      assert.ok(
        quickProjectionReturn < 500,
        `A prefetched projection renders before its delayed refresh: ${quickProjectionReturn}ms`,
      );
      assert.ok(
        syncPulls > pullsBeforeQuickReturn,
        "The fast cached return still starts its background refresh",
      );
      projectionDelay = 0;
      await open(b.id, "sync-b-29");
      projectionDelay = 1000;
      hold = true;
      const syncCached = await open(a.id, "sync-a-29");
      assert.ok(
        syncCached < 500,
        `Revisit does not wait for RxDB pull: ${syncCached}ms`,
      );
      assert.ok(
        syncPulls >= 2,
        "Real RxDB replication loaded both chat projections",
      );
      assert.equal(
        await activePage.locator('[data-message="sync-b-29"]').count(),
        0,
        "Replicated chat history stays scoped",
      );
      const sends: PostBody<"/api/messages">[] = [];
      await activePage.route("**/api/messages", async (r) => {
        const body = fixturePostBody(
          "/api/messages",
          r.request().postDataJSON(),
        );
        sends.push(body);
        assert.equal(
          r.request().headers()["x-canvas-workspace"],
          identity.workspaceId,
          "Send uses the synchronized workspace",
        );
        await r.fulfill({
          json: {
            id: body.id,
            status: "queued",
            deliveries: { [a.id]: "queued" },
          } satisfies Pick<
            PostResult<"/api/messages">,
            "id" | "status" | "deliveries"
          >,
        });
      });
      await activePage.locator("#stop").waitFor();
      for (const action of ["button", "Enter"]) {
        await activePage.locator("#message").fill(`Sync shortcut ${action}`);
        if (action === "button") await activePage.locator("#send").click();
        else await activePage.locator("#message").press(action);
        await page
          .waitForFunction(
            () =>
              document.querySelector<HTMLTextAreaElement>("#message")?.value ===
              "",
          )
          .catch(async (error) => {
            console.error("Shortcut failure", action, JSON.stringify(sends));
            throw error;
          });
        const expectedCount = action === "button" ? 1 : 2;
        const deadline = Date.now() + 12000;
        while (sends.length < expectedCount && Date.now() < deadline)
          await new Promise((resolve) => setTimeout(resolve, 40));
        assert.equal(
          sends.length,
          expectedCount,
          "The outbox submits each message once",
        );
        assert.equal(sends.at(-1)?.delivery, "current_turn");
        assert.equal(sends.at(-1)?.room, a.id);
      }
      assert.equal(sends.length, 2, "Each shortcut sends exactly one message");
      assert.equal(
        new Set(sends.map((x) => x.id)).size,
        2,
        "Distinct intentions retain distinct message identities",
      );
      assert.equal(
        legacyDraftConflicts,
        0,
        "Sequential edit and clear do not create a false conflict on the old server",
      );
      assert.deepEqual(errors, []);
      console.log(
        `Chat switch latency: PASS (cold ${cold}ms, cached ${cached}ms, refreshed ${refreshed}ms, RxDB revisit ${syncCached}ms; scope isolation, scroll restore, late HTTP guard, real RxDB Send/Enter).`,
      );
    } catch (e) {
      await page?.screenshot({ path: join(dir, "failure.png") });
      console.error("Evidence:", dir);
      throw e;
    } finally {
      await Promise.all(
        testBrowser
          .contexts()
          .filter((ownedContext) => ownedContext !== runnerContext)
          .map((ownedContext) => ownedContext.close()),
      );
    }
  },
);
