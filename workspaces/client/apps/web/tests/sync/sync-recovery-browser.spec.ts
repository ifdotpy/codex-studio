import type { FixtureResourceChange } from "./helpers.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import { fixturePostBody } from "./helpers.ts";
import type {} from "./sync-recovery-browser.fixture.tsx";
import { fixtureOrigin } from "./helpers.ts";
import {
  apiSchemaHeaders,
  apiSchemaHandshakeSse,
  protocol3SseEvent,
  syncIdentityFixture,
  test,
} from "../playwright.ts";
// Exercise reconnect and delivery state in real React/RxDB browser hooks.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
test("Sync recovery browser", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const server = await createServer({
    configFile: false,
    optimizeDeps: { noDiscovery: true },
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const page = runnerPage,
      errors: string[] = [];
    page.on("pageerror", (error) =>
      errors.push(error instanceof Error ? error.message : String(error)),
    );
    let identityStatus = 503,
      hangSession = false,
      failPull = false,
      nextStreamResourceEvent = false,
      streamRevision = 0,
      badReceipt = true;
    let workspaceId = "c".repeat(32);
    const sends: PostBody<"/api/messages">[] = [];
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Recovery contract</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        status: identityStatus,
        headers: apiSchemaHeaders(),
        json:
          identityStatus === 200
            ? syncIdentityFixture(workspaceId)
            : { error: "Connection unavailable" },
      }),
    );
    await page.route("**/api/session", (route) =>
      hangSession ? undefined : route.fulfill({ json: { token: "fixture" } }),
    );
    await page.route("**/api/sync/stream*", (route) => {
      let body = "";
      if (nextStreamResourceEvent) {
        nextStreamResourceEvent = false;
        streamRevision++;
        body = protocol3SseEvent("resources", {
          protocol: 4,
          workspaceId,
          epoch: "recovery-fixture",
          revision: streamRevision,
          reason: "change",
          resources: [{ kind: "state" }],
        } satisfies FixtureResourceChange);
      }
      return route.fulfill({
        contentType: "text/event-stream",
        headers: apiSchemaHeaders({ "content-type": "text/event-stream" }),
        body: apiSchemaHandshakeSse(body),
      });
    });
    await page.route("**/api/sync/pull?*", (route) => {
      const after = Number(
        new URL(route.request().url()).searchParams.get("after"),
      );
      return route.fulfill({
        status: failPull ? 503 : 200,
        headers: apiSchemaHeaders(),
        json: failPull
          ? { error: "Pull unavailable" }
          : {
              workspaceId,
              generation: 1,
              documents: after
                ? []
                : [
                    {
                      op: "upsert",
                      collection: "workspace",
                      id: "current",
                      value: {
                        stateDir: "fixture-workspace",
                        marker: "replicated",
                      },
                      seq: 1,
                    },
                  ],
              checkpoint: { seq: 1 },
              maxSeq: 1,
              initialHigh: 1,
            },
      });
    });
    await page.route("**/api/messages", (route) => {
      const body = fixturePostBody<"/api/messages">(
        route.request().postDataJSON(),
      );
      sends.push(body);
      return route.fulfill({
        json: {
          id: badReceipt ? "wrong-message" : body.id,
          status: "accepted",
        },
      });
    });
    await page.route("**/hang", () => {});
    await page.goto(fixtureOrigin(server) + `/check`);
    await page.addScriptTag({
      type: "module",
      url: "/tests/sync/sync-recovery-browser.fixture.tsx",
    });
    await page.evaluate(() => window.syncRecoveryBrowserMount1());
    await page.waitForFunction(
      () =>
        window.syncRecoveryBrowserFixtureSnapshot?.error &&
        window.syncRecoveryBrowserFixtureOutbox?.error,
    );
    identityStatus = 200;
    await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    await page.waitForFunction(() => {
      const first = window.syncRecoveryBrowserFixtureSnapshot.data?.runtime;
      const second = window.syncRecoveryBrowserFixtureSecond.data?.runtime;
      return (
        first &&
        "marker" in first &&
        first.marker === "replicated" &&
        second &&
        "marker" in second &&
        second.marker === "replicated" &&
        !window.syncRecoveryBrowserFixtureSnapshot.error &&
        !window.syncRecoveryBrowserFixtureOutbox.error
      );
    });
    failPull = true;
    nextStreamResourceEvent = true;
    await page.evaluate(() => {
      window.dispatchEvent(new Event("offline"));
      window.dispatchEvent(new Event("online"));
    });
    await page.waitForFunction(
      () =>
        window.syncRecoveryBrowserFixtureSnapshot.error ===
          "Pull unavailable" &&
        window.syncRecoveryBrowserFixtureSecond.error === "Pull unavailable",
    );
    failPull = false;
    nextStreamResourceEvent = true;
    await page.evaluate(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await page.waitForFunction(
      () =>
        !window.syncRecoveryBrowserFixtureSnapshot.error &&
        !window.syncRecoveryBrowserFixtureSecond.error,
    );
    assert.equal(
      await page.evaluate(() =>
        window.syncRecoveryBrowserFixtureSnapshot.data?.runtime &&
        "marker" in window.syncRecoveryBrowserFixtureSnapshot.data.runtime
          ? window.syncRecoveryBrowserFixtureSnapshot.data.runtime.marker
          : undefined,
      ),
      "replicated",
      "An empty pull clears stale errors without a new snapshot",
    );
    hangSession = true;
    const timeout = await page.evaluate(async () => {
      const { get } = await import("/src/api.ts");
      const start = performance.now();
      try {
        await get("/api/session", { timeoutMs: 100 });
        return null;
      } catch (error) {
        return {
          name: error instanceof Error ? error.name : "",
          message: error instanceof Error ? error.message : String(error),
          elapsed: performance.now() - start,
        };
      }
    });
    hangSession = false;
    assert.ok(timeout);
    assert.equal(
      timeout.name,
      "NetworkTimeoutError",
      `Timeout result: ${JSON.stringify(timeout)}`,
    );
    assert.ok(timeout.elapsed < 2000);
    const body = {
      id: "receipt-check",
      room: "lead",
      text: "Keep this exact request",
      assets: [],
      delivery: "queue",
    } satisfies Parameters<
      typeof import("../../src/sync/send.ts").durableSend
    >[0];
    const first = await page.evaluate(
      (body) => window.syncRecoveryBrowserFixtureDurableSend(body),
      body,
    );
    assert.equal(
      first.queued,
      true,
      "An unrelated receipt must not confirm delivery",
    );
    await page.waitForFunction(() =>
      window.syncRecoveryBrowserFixtureOutbox.entries.some(
        (entry) => entry.id === "receipt-check" && entry.status === "queued",
      ),
    );
    badReceipt = false;
    await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    await page.waitForFunction(() =>
      window.syncRecoveryBrowserFixtureOutbox.entries.some(
        (entry) => entry.id === "receipt-check" && entry.status === "accepted",
      ),
    );
    assert.ok(sends.length >= 2);
    assert.ok(
      sends.every((sent) => JSON.stringify(sent) === JSON.stringify(body)),
    );
    // The queue drains by creation time, even when document IDs sort differently.
    const beforeOrder = sends.length;
    await page.evaluate(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      for (const [id, created] of [
        ["z-first", 10],
        ["a-second", 20],
      ] as const)
        await db.outbox.insert({
          id,
          seq: 0,
          payload: JSON.stringify({
            body: { id, room: "lead", text: id },
            status: "queued",
            created,
          }),
        });
      window.dispatchEvent(new Event("pageshow"));
    });
    await page.waitForFunction(() =>
      ["z-first", "a-second"].every((id) =>
        window.syncRecoveryBrowserFixtureOutbox.entries.some(
          (entry) => entry.id === id && entry.status === "accepted",
        ),
      ),
    );
    assert.deepEqual(
      sends.slice(beforeOrder).map((body) => body.id),
      ["z-first", "a-second"],
    );
    const beforeConcurrent = sends.length;
    await page.evaluate(async () => {
      const body = { id: "same-request", room: "lead", text: "Same intention" };
      await Promise.all([
        window.syncRecoveryBrowserFixtureDurableSend(body),
        window.syncRecoveryBrowserFixtureDurableSend(body),
      ]);
    });
    assert.equal(
      sends.length,
      beforeConcurrent + 1,
      "Concurrent calls share one delivery task",
    );
    const collisions = await page.evaluate(() =>
      Promise.allSettled([
        window.syncRecoveryBrowserFixtureDurableSend({
          id: "collision",
          room: "lead",
          text: "First text",
        }),
        window.syncRecoveryBrowserFixtureDurableSend({
          id: "collision",
          room: "lead",
          text: "Different text",
        }),
      ]).then((results) => results.map((result) => result.status)),
    );
    assert.deepEqual(collisions.sort(), ["fulfilled", "rejected"]);
    // A new workspace cannot receive the old workspace's pending messages.
    workspaceId = "d".repeat(32);
    const beforeSwitch = sends.length;
    const rejected = await page.evaluate(async () => {
      try {
        await window.syncRecoveryBrowserFixtureDurableSend({
          id: "old-workspace",
          room: "lead",
          text: "Do not cross workspaces",
        });
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    });
    assert.ok(rejected);
    assert.match(rejected, /workspace changed/);
    assert.equal(sends.length, beforeSwitch);
    assert.deepEqual(errors, []);
    console.log(
      "PASS: startup recovery, PWA resume, empty pull recovery for shared subscribers, deadlines, exact receipts, ordered queue, workspace isolation",
    );
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    await server.close();
  }
});
