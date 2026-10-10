import {
  readTestState,
  stubEntityState,
  readFixtureSyncContract,
  readApiSchemaHash,
  test,
  spawnFixture as spawn,
} from "../playwright.mjs";
// Real renderer and isolated server. Delayed streams must not delay navigation.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test(
  "Chat switch latency ui",
  { tag: "@performance" },
  async ({
    browser: testBrowser,
    page: runnerPage,
    context: runnerContext,
  }) => {
    test.setTimeout(300_000);
    const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
    const dir = await mkdtemp(join(tmpdir(), "studio-chat-switch-"));
    const proc = spawn(
      "python3",
      [
        "-B",
        join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
        dir,
      ],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    let log = "",
      page;
    proc.stderr.on("data", (d) => (log += d));
    try {
      const port = await new Promise((resolve, reject) => {
        proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
        proc.once("exit", () => reject(Error(log)));
      });
      const origin = `http://127.0.0.1:${port}`;
      const state = await readTestState(origin);
      const a = state.threads.find((x) => x.name === "Other project");
      const b = state.threads.find((x) => x.name === "Release lead");
      page = runnerPage;
      await page.setViewportSize({ width: 1440, height: 960 });
      page.setDefaultTimeout(10000);
      const errors = [];
      page.on("pageerror", (e) => errors.push(e.message));
      await page.addInitScript(() => {
        window.__chatSwitchStartedAt = undefined;
        window.__chatSwitchFirstRowsPaintAt = undefined;
        window.__chatSwitchPaintAt = undefined;
        window.__chatSwitchScrollStartedAt = undefined;
        window.__chatSwitchLongTasks = [];
        window.__firstSidebarRowVisibleAt = undefined;
        const sidebarObserver = new MutationObserver(() => {
          const row = document.querySelector("[data-chat]");
          if (row?.getClientRects().length) {
            window.__firstSidebarRowVisibleAt = performance.now();
            sidebarObserver.disconnect();
          }
        });
        sidebarObserver.observe(document, { childList: true, subtree: true });
        new PerformanceObserver((list) => {
          for (const entry of list.getEntries())
            window.__chatSwitchLongTasks.push({
              startTime: entry.startTime,
              duration: entry.duration,
            });
        }).observe({ type: "longtask", buffered: true });
      });
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
      const payload = (agent, tag) => ({
        agent: { ...agent, status: "completed", inFlight: false, turnId: null },
        items: Array.from({ length: 30 }, (_, i) => ({
          id: `${tag}-${i}`,
          role: "assistant",
          text: `History ${tag} ${i}. ${"Visible saved text. ".repeat(15)}`,
        })),
        replace: true,
      });
      const requests = [];
      const visited = new Set();
      let hold = false;
      let coldSwitchArmed = false;
      let resolveNextA;
      let resolveHeldB;
      let latestA = payload(a, "a");
      let latestASequence = 10;
      const responseFor = (id, data, seq, after = 0) => ({
        ...syncContract.identity,
        documents:
          after < seq
            ? [
                {
                  id: `transcript:${id}`,
                  payload: JSON.stringify(data),
                  seq,
                  _deleted: false,
                },
              ]
            : [],
        checkpoint: { seq: Math.max(after, seq) },
      });
      await page.route("**/api/sync/pull?*", async (r) => {
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
      await page.goto(origin);
      await page.waitForFunction(() =>
        performance.getEntriesByType("resource").some((entry) => {
          const url = new URL(entry.name);
          return url.pathname === "/api/session" && entry.responseEnd > 0;
        }),
      );
      await page.locator("#conversation-title").waitFor();
      await page.locator(`[data-chat="${b.id}"]`).waitFor();
      await page.locator(`[data-chat="${b.id}"]`).click();
      await page
        .locator("#conversation-title")
        .getByText("Release lead", { exact: true })
        .waitFor();
      await page.locator("#messages [data-message^='b-']").first().waitFor();
      console.log(
        "chat-switch startup readiness",
        JSON.stringify(
          await page.evaluate(() => {
            const resources = performance
              .getEntriesByType("resource")
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
              firstSidebarRowAt: window.__firstSidebarRowVisibleAt,
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
      const open = async (id, message) => {
        const chat = page.locator(`[data-chat="${id}"]`);
        const selector = `[data-message="${message}"]`;
        const prefix = message.slice(0, message.lastIndexOf("-"));
        await chat.evaluate((element, target) => {
          window.__chatSwitchStartedAt = undefined;
          window.__chatSwitchFirstRowsPaintAt = undefined;
          window.__chatSwitchPaintAt = undefined;
          window.__chatSwitchScrollStartedAt = undefined;
          element.addEventListener(
            "pointerdown",
            () => {
              window.__chatSwitchStartedAt = performance.now();
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
                  window.__chatSwitchFirstRowsPaintAt = performance.now();
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
                  window.__chatSwitchPaintAt = performance.now();
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
        if (id === a.id && !visited.has(id)) coldSwitchArmed = true;
        await chat.click();
        await page.waitForFunction(
          () => typeof window.__chatSwitchFirstRowsPaintAt === "number",
        );
        const firstRowsTiming = await page.evaluate((id) => {
          const startedAt = window.__chatSwitchStartedAt;
          const firstRowsAt = window.__chatSwitchFirstRowsPaintAt;
          const resource = performance
            .getEntriesByType("resource")
            .filter((entry) => {
              const url = new URL(entry.name);
              return (
                url.pathname === "/api/sync/pull" &&
                url.searchParams.get("scope") === `transcript:${id}` &&
                entry.startTime >= startedAt &&
                entry.startTime <= firstRowsAt
              );
            })
            .sort((left, right) => left.startTime - right.startTime)[0];
          return {
            clickToFirstRowsMs: firstRowsAt - startedAt,
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
              .filter((entry) => {
                const url = new URL(entry.name);
                return (
                  url.pathname.startsWith("/api/") &&
                  entry.startTime >= startedAt &&
                  entry.startTime <= firstRowsAt
                );
              })
              .map((entry) => ({
                path: new URL(entry.name).pathname,
                startAfterClickMs: entry.startTime - startedAt,
                durationMs: entry.duration,
              })),
            longTasks: window.__chatSwitchLongTasks
              .filter(
                (entry) =>
                  entry.startTime >= startedAt &&
                  entry.startTime <= firstRowsAt,
              )
              .map((entry) => ({
                startAfterClickMs: entry.startTime - startedAt,
                durationMs: entry.duration,
              })),
          };
        }, id);
        if (!visited.has(id)) {
          await page.locator("#messages").evaluate((messages) => {
            window.__chatSwitchScrollStartedAt = performance.now();
            messages.scrollTop = messages.scrollHeight;
            messages.dispatchEvent(new Event("scroll"));
          });
          visited.add(id);
        }
        await page.locator(selector).waitFor({ state: "visible" });
        const targetMountAfterScrollMs = await page.evaluate(() =>
          typeof window.__chatSwitchScrollStartedAt === "number"
            ? performance.now() - window.__chatSwitchScrollStartedAt
            : null,
        );
        const timing = {
          id,
          ...firstRowsTiming,
          scrollAndTargetMountMs: targetMountAfterScrollMs,
        };
        console.log("chat-switch timing", JSON.stringify(timing));
        return firstRowsTiming.clickToFirstRowsMs;
      };
      await page
        .locator("#conversation-title")
        .getByText("Release lead", { exact: true })
        .waitFor();
      await page.locator("#messages [data-message^='b-']").first().waitFor();
      await page.waitForFunction(() => {
        const entries = performance.getEntriesByType("resource");
        return entries.some((entry) => {
          const url = new URL(entry.name);
          return url.pathname === "/api/session" && entry.responseEnd > 0;
        });
      });
      const cold = await open(a.id, "a-29");
      assert.ok(
        cold < 1500,
        `Cold history appears without the old three-second delay: ${cold}ms`,
      );
      await page.locator("#messages").evaluate((e) => {
        e.scrollTop = 950;
        e.dispatchEvent(new Event("scroll"));
      });
      const savedTop = await page
        .locator("#messages")
        .evaluate((e) => e.scrollTop);
      await open(b.id, "b-29");
      hold = true;
      const previousReads = requests.filter((x) => x.id === a.id).length;
      const refresh = new Promise((resolve) => (resolveNextA = resolve));
      const cached = await open(a.id, "a-29");
      assert.ok(
        cached < 500,
        `Saved history appears before a blocked refresh: ${cached}ms`,
      );
      assert.equal(
        await page.locator('[data-message="b-29"]').count(),
        0,
        "No other chat history leaks",
      );
      assert.ok(
        Math.abs(
          (await page.locator("#messages").evaluate((e) => e.scrollTop)) -
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
      const freshPull = page.waitForResponse((response) => {
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
      await page.locator("#messages").evaluate((messages) => {
        messages.scrollTop = messages.scrollHeight;
        messages.dispatchEvent(new Event("scroll"));
      });
      await page.locator('[data-message="fresh-29"]').waitFor();
      await page.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
      assert.equal(
        await page.locator('[data-message="obsolete-29"]').count(),
        0,
        "Delayed HTTP cannot replace a newer stream",
      );
      const heldB = new Promise((resolve) => (resolveHeldB = resolve));
      const lateRequest = page.waitForRequest((request) => {
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
      await page.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
      assert.equal(
        await page.locator('[data-message="late-other-29"]').count(),
        0,
        "Response from a previous chat cannot replace current history",
      );
      // Exercise the real RxDB projection path, including a slow pull on revisit.
      await page.unroute("**/api/sync/**");
      const identity = await (
        await fetch(origin + "/api/sync/identity")
      ).json();
      let projectionDelay = 0;
      let syncPulls = 0;
      const waitForProjection = async (id, sequence) =>
        page.waitForFunction(
          async ({ databaseName, documentId, sequence }) => {
            const request = indexedDB.open(databaseName);
            const db = await new Promise((resolve, reject) => {
              request.onsuccess = () => resolve(request.result);
              request.onerror = () => reject(request.error);
            });
            const matches = await Promise.all(
              [...db.objectStoreNames].map(
                (store) =>
                  new Promise((resolve) => {
                    const query = db
                      .transaction(store, "readonly")
                      .objectStore(store)
                      .getAll();
                    query.onsuccess = () =>
                      resolve(
                        query.result.some(
                          (row) =>
                            row.id === documentId && row.seq === sequence,
                        ),
                      );
                    query.onerror = () => resolve(false);
                  }),
              ),
            );
            db.close();
            return matches.some(Boolean);
          },
          {
            databaseName: `rxdb-dexie-studio-entity-projection-${identity.workspaceId}-${readApiSchemaHash()}--0--projections`,
            documentId: `transcript:${id}`,
            sequence,
          },
        );
      await page.route("**/api/sync/pull?*", async (r) => {
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
        data.agent = {
          ...data.agent,
          status: "running",
          inFlight: true,
          turnId: "fixture-live-turn",
        };
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
          },
        });
      });
      // Preserve the old server's byte comparison in this fixture. New UI writes
      // must clear drafts safely before a busy backend can be upgraded.
      let legacyDraftConflicts = 0;
      await page.route("**/api/sync/drafts", async (r) => {
        const old = await (
          await fetch(origin + "/api/sync/pull?scope=drafts")
        ).json();
        const conflicts = [];
        for (const row of r.request().postDataJSON().rows) {
          const next = row.newDocumentState;
          const previous = old.documents.find((x) => x.id === next.id);
          const assumed = row.assumedMasterState;
          if (
            previous &&
            (!assumed ||
              assumed.payload !== previous.payload ||
              !!assumed._deleted !== !!previous._deleted) &&
            (next.payload !== previous.payload ||
              !!next._deleted !== !!previous._deleted)
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
      await page.reload();
      await open(a.id, "sync-a-29");
      projectionDelay = 1000;
      await page.locator(`[data-chat="${b.id}"]`).click();
      await page.locator('[data-message="sync-b-29"]').waitFor();
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
        await page.locator('[data-message="sync-b-29"]').count(),
        0,
        "Replicated chat history stays scoped",
      );
      const sends = [];
      await page.route("**/api/messages", async (r) => {
        sends.push(r.request().postDataJSON());
        assert.equal(
          r.request().headers()["x-canvas-workspace"],
          identity.workspaceId,
          "Send uses the synchronized workspace",
        );
        await r.fulfill({
          json: {
            id: r.request().postDataJSON().id,
            status: "queued",
            deliveries: { [a.id]: "queued" },
          },
        });
      });
      await page.locator("#stop").waitFor();
      for (const action of ["button", "Enter"]) {
        await page.locator("#message").fill(`Sync shortcut ${action}`);
        if (action === "button") await page.locator("#send").click();
        else await page.locator("#message").press(action);
        await page
          .waitForFunction(
            () => document.querySelector("#message").value === "",
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
        assert.equal(sends.at(-1).delivery, "after_tool");
        assert.equal(sends.at(-1).room, a.id);
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
