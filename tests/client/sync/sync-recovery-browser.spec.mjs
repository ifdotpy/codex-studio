import { test } from "../playwright.mjs";
// Exercise reconnect and delivery state in real React/RxDB browser hooks.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
test("Sync recovery browser", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
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
  await server.listen();
  try {
    const page = runnerPage,
      errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let identityStatus = 503,
      hangSession = false,
      failPull = false,
      badReceipt = true;
    let workspaceId = "c".repeat(32);
    const sends = [];
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Recovery contract</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({
        status: identityStatus,
        json:
          identityStatus === 200
            ? { workspaceId }
            : { error: "Connection unavailable" },
      }),
    );
    await page.route(/\/api\/state(?:\?.*)?$/, (route) =>
      route.fulfill({
        status: identityStatus,
        json:
          identityStatus === 200
            ? {
                token: "fixture",
                stateDir: "fixture-workspace",
                threads: [],
                chats: [],
                nodes: [],
                edges: [],
                at: 1,
                runtime: {
                  agents: [],
                  complaints: [],
                  connected: false,
                  events: [],
                  monitors: [],
                  nativeNotices: [],
                  peerTeams: [],
                  peerTeamsVersion: 1,
                  projectOrganizationVersion: 1,
                  projects: [],
                  rateLimits: { accountKey: "default", at: null, data: {} },
                  rateLimitsByAccount: {},
                  requests: [],
                  rooms: [],
                  rules: [],
                  tasks: [],
                  tasksHistoryLimit: 100,
                  marker: "http",
                },
              }
            : { error: "Connection unavailable" },
      }),
    );
    await page.route("**/api/session", (route) =>
      hangSession ? undefined : route.fulfill({ json: { token: "fixture" } }),
    );
    await page.route("**/api/sync/stream*", (route) =>
      route.fulfill({ contentType: "text/event-stream", body: "" }),
    );
    await page.route("**/api/sync/pull?*", (route) => {
      const after = Number(
        new URL(route.request().url()).searchParams.get("after"),
      );
      return route.fulfill({
        status: failPull ? 503 : 200,
        json: failPull
          ? { error: "Pull unavailable" }
          : {
              workspaceId,
              generation: 1,
              documents: after
                ? []
                : [
                    {
                      id: "entity:workspace:current",
                      seq: 1,
                      payload: JSON.stringify({
                        collection: "workspace",
                        id: "current",
                        value: {
                          stateDir: "fixture-workspace",
                          marker: "replicated",
                        },
                      }),
                      _deleted: false,
                    },
                  ],
              checkpoint: { seq: 1 },
              maxSeq: 1,
              initialHigh: 1,
            },
      });
    });
    await page.route("**/api/messages", (route) => {
      const body = route.request().postDataJSON();
      sends.push(body);
      return route.fulfill({
        json: {
          id: badReceipt ? "wrong-message" : body.id,
          status: "accepted",
        },
      });
    });
    await page.route("**/hang", () => {});
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(async () => {
      const { useSnapshot } = await import("/src/hooks.ts");
      const send = await import("/src/sync/send.ts");
      window.durableSend = send.durableSend;
      const r = await import("/node_modules/.vite/deps/react.js"),
        d = await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = r.default || r,
        { createRoot } = d.default || d;
      const node = document.createElement("div");
      document.body.appendChild(node);
      createRoot(node).render(
        React.createElement(function Harness() {
          window.snapshot = useSnapshot();
          window.second = useSnapshot();
          window.outbox = send.useOutbox();
          return null;
        }),
      );
    });
    await page.waitForFunction(
      () => window.snapshot?.error && window.outbox?.error,
    );
    identityStatus = 200;
    const previousSnapshotError = await page.evaluate(
      () => window.snapshot.error,
    );
    await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    await page.waitForFunction(
      (previousError) =>
        (window.snapshot.data?.runtime?.marker === "replicated" &&
          window.second.data?.runtime?.marker === "replicated" &&
          !window.snapshot.error &&
          !window.outbox.error) ||
        window.snapshot.error !== previousError,
      previousSnapshotError,
    );
    assert.equal(
      await page.evaluate(() => window.snapshot.data?.runtime?.marker),
      "replicated",
      "The entity pull must replace the valid legacy state fallback",
    );
    failPull = true;
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await page.waitForFunction(
      () =>
        window.snapshot.error === "Pull unavailable" &&
        window.second.error === "Pull unavailable",
    );
    failPull = false;
    await page.evaluate(() =>
      document.dispatchEvent(new Event("visibilitychange")),
    );
    await page.waitForFunction(
      () => !window.snapshot.error && !window.second.error,
    );
    assert.equal(
      await page.evaluate(() => window.snapshot.data.runtime.marker),
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
          name: error.name,
          message: error.message,
          elapsed: performance.now() - start,
        };
      }
    });
    hangSession = false;
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
    };
    const first = await page.evaluate((body) => window.durableSend(body), body);
    assert.equal(
      first.queued,
      true,
      "An unrelated receipt must not confirm delivery",
    );
    await page.waitForFunction(() =>
      window.outbox.entries.some(
        (entry) => entry.id === "receipt-check" && entry.status === "queued",
      ),
    );
    badReceipt = false;
    await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
    await page.waitForFunction(() =>
      window.outbox.entries.some(
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
      ])
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
        window.outbox.entries.some(
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
      await Promise.all([window.durableSend(body), window.durableSend(body)]);
    });
    assert.equal(
      sends.length,
      beforeConcurrent + 1,
      "Concurrent calls share one delivery task",
    );
    const collisions = await page.evaluate(() =>
      Promise.allSettled([
        window.durableSend({
          id: "collision",
          room: "lead",
          text: "First text",
        }),
        window.durableSend({
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
        await window.durableSend({
          id: "old-workspace",
          room: "lead",
          text: "Do not cross workspaces",
        });
      } catch (error) {
        return error.message;
      }
    });
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
