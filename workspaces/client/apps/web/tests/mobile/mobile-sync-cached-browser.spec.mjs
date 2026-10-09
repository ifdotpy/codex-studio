// Cached UI appears while an online Mac is unreachable; identity gates replication.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import { apiSchemaHandshakeSse, test } from "../playwright.mjs";

const browserContextsByTest = new WeakMap();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("mobile sync cached browser", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const { createServer } = await import(
    new URL("../../node_modules/vite/dist/node/index.js", import.meta.url)
  );
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  const browser = _browser;
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const workspaceId = "f".repeat(32);
    let identityCalls = 0;
    let currentWorkspace = workspaceId;
    let holdIdentity = false,
      identityRoute,
      pullCount = 0,
      pushCount = 0;
    const pullLog = [];
    let streamCalls = 0;
    await page.route("**/check", (route) =>
      route.fulfill({ contentType: "text/html", body: "<!doctype html>" }),
    );
    await page.route("**/api/sync/identity", (route) => {
      identityCalls++;
      if (holdIdentity) {
        identityRoute = route;
        return;
      }
      return route.fulfill({
        json: { workspaceId: currentWorkspace, chatState: true },
      });
    });
    await page.route("**/api/sync/stream*", (route) => {
      streamCalls++;
      return route.fulfill({
        contentType: "text/event-stream",
        body: apiSchemaHandshakeSse(),
      });
    });
    await page.route("**/api/sync/pull?*", (route) => {
      pullCount++;
      const url = new URL(route.request().url());
      const scope = url.searchParams.get("scope");
      const after = Number(url.searchParams.get("after"));
      pullLog.push({ scope, after, currentWorkspace });
      return route.fulfill({
        json: {
          workspaceId,
          documents:
            scope === "state:entities:v1" && after < 2
              ? [
                  {
                    id: "entity:agent:lead",
                    seq: 2,
                    payload: JSON.stringify({
                      collection: "agent",
                      id: "lead",
                      value: {
                        id: "lead",
                        name: "Fresh",
                        status: "running",
                        source: "managed",
                      },
                    }),
                  },
                ]
              : [],
          checkpoint: { seq: 2 },
          initialHigh: 2,
          maxSeq: 2,
        },
      });
    });
    await page.route("**/api/sync/drafts", (route) => {
      pushCount++;
      return route.fulfill({ json: [] });
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(async () => {
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      await db.projections.insert({
        id: "entity:agent:lead",
        seq: 1,
        payload: JSON.stringify({
          collection: "agent",
          id: "lead",
          value: {
            id: "lead",
            name: "Cached",
            status: "running",
            source: "managed",
          },
        }),
      });
      await db.projections.insert({
        id: "state:entities:ready",
        seq: 1,
        payload: "ready",
      });
      await db.drafts.insert({
        id: "phone:lead",
        seq: 0,
        payload: JSON.stringify({
          id: "phone:lead",
          text: "Never send to another workspace",
          session: "lead",
          device: "phone",
          updated: 1,
        }),
      });
    });
    holdIdentity = true;
    await page.reload();
    const started = Date.now();
    const startWatchers = () =>
      page.evaluate(async () => {
        const client = await import("/src/sync/client.ts");
        window.syncErrors = [];
        window.stopState = client.subscribeStateProjection(
          (value) => (window.state = value),
          (error) => {
            if (error) window.syncErrors.push(String(error));
          },
        );
        window.stopDrafts = await client.startDraftReplication((error) => {
          if (error) window.syncErrors.push(String(error));
        });
      });
    await startWatchers();
    await page.waitForFunction(
      () => window.state?.runtime?.agents?.[0]?.name === "Cached",
    );
    const cachedMs = Date.now() - started;
    assert.ok(
      cachedMs < 1500,
      `Cached state waits for the Mac: ${cachedMs} ms`,
    );
    assert.ok(
      await page.evaluate(() => navigator.onLine),
      "The test reproduces an online phone with an unreachable Mac",
    );
    assert.equal(
      pullCount,
      0,
      "Unverified cached identity permits no projection reads",
    );
    assert.equal(
      pushCount,
      0,
      "Unverified cached identity permits no draft writes",
    );
    assert.ok(identityRoute);
    holdIdentity = false;
    currentWorkspace = "a".repeat(32);
    await identityRoute.fulfill({
      json: { workspaceId: "a".repeat(32), chatState: true },
    });
    await page.waitForFunction(() =>
      window.syncErrors.some((error) => error.includes("workspace changed")),
    );
    assert.equal(
      pullCount,
      0,
      "A different server cannot enter the cached database",
    );
    assert.equal(
      pushCount,
      0,
      "A different server cannot receive cached drafts",
    );
    assert.equal(
      await page.evaluate(() => window.state.runtime.agents[0].name),
      "Cached",
    );
    assert.ok(
      (await page.evaluate(() => window.syncErrors)).some((error) =>
        error.includes("workspace changed"),
      ),
      "A workspace identity mismatch blocks cached data access",
    );

    // A changed server requires a reload; the saved workspace can then recover.
    currentWorkspace = workspaceId;
    await page.reload();
    await startWatchers();
    await page
      .waitForFunction(
        () => window.state?.runtime?.agents?.[0]?.name === "Fresh",
        null,
        {
          timeout: 10000,
        },
      )
      .catch(async (error) => {
        console.error({
          identityCalls,
          pullCount,
          pushCount,
          streamCalls,
          pullLog,
          currentWorkspace,
          state: await page.evaluate(async () => {
            const { db } = await (
              await import("/src/sync/client.ts")
            ).syncDatabase();
            const rows = await db.projections.find().exec();
            return {
              value: window.state,
              errors: window.syncErrors,
              hidden: document.hidden,
              online: navigator.onLine,
              entities: rows
                .filter((row) => row.id.startsWith("entity:"))
                .map((row) => [
                  row.id,
                  row.seq,
                  JSON.parse(row.payload).value?.name,
                ]),
              ready: (
                await db.projections.findOne("state:entities:ready").exec()
              )?.payload,
            };
          }),
        });
        throw error;
      });
    for (let attempt = 0; attempt < 100 && !pushCount; attempt++)
      await new Promise((resolve) => setTimeout(resolve, 30));
    assert.ok(
      pushCount > 0,
      "Draft replication resumes after exact workspace verification",
    );
    assert.deepEqual(errors, []);
    console.log(
      `PASS: cached online startup ${cachedMs} ms, blocked cross-workspace reads/writes, verified retry recovery`,
    );
  } finally {
    await server.close();
  }
});
