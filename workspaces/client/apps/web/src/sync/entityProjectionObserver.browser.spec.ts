import type { LegacySyncIdentityResponse } from "./syncBrowserFixture.ts";
import {
  openProjection,
  startObserver,
} from "./entityProjectionObserver.browser.fixture.ts";
import { fixturePort } from "./syncBrowserFixture.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  test,
} from "../../tests/playwright.ts";

test("entity observer applies store changes and resets before publication", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  const { createFixtureViteServer: createServer } =
    await import("../../tests/playwright.ts");
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  const context = await browser.newContext();
  try {
    await server.listen();
    await context.route("**/observer-check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Entity observer check</title>",
      }),
    );
    await context.route("**/api/sync/identity", (route) =>
      route.fulfill({
        json: {
          workspaceId: "a".repeat(32),
        } satisfies LegacySyncIdentityResponse,
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
      }),
    );
    const origin = `http://127.0.0.1:${fixturePort(server)}`;
    const page = await context.newPage();
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(`${origin}/observer-check`);
    await page.evaluate(openProjection);
    await page.evaluate(startObserver);
    await page.waitForFunction(
      () => window.snapshots.at(-1)?.threads.length === 2000,
    );
    await page.evaluate(async () => {
      window.beforeBurst = window.snapshots.length;
      window.beforeBurstReads = window.observerReads.length;
      window.burstStartedAt = performance.now();
      await window.putRows(
        Array.from({ length: 100 }, (_, index) =>
          window.entityRow(`${index}`, 2001 + index, `changed-${index}`),
        ),
      );
    });
    await page.waitForFunction(
      () =>
        window.snapshots.at(-1)?.threads.find((agent) => agent.id === "99")
          ?.name === "changed-99",
    );
    const burst = await page.evaluate(() => {
      const initial = window.snapshots[0];
      const latest = window.snapshots.at(-1);
      const readIds = window.observerReads.at(-1);
      if (!initial || !latest || !readIds)
        throw new Error("Expected observer snapshots and storage reads");
      return {
        publications: window.snapshots.length - window.beforeBurst,
        queries: window.queryCalls,
        unchanged: initial.threads[100] === latest.threads[100],
        storageReads: window.observerReads.length - window.beforeBurstReads,
        readIds: readIds.length,
        elapsedMs: window.lastPublishedAt - window.burstStartedAt,
      };
    });
    assert.equal(burst.publications, 1);
    assert.equal(burst.queries, 1, JSON.stringify(burst));
    assert.equal(burst.unchanged, true);
    assert.equal(burst.storageReads, 1);
    // 100 changed rows, the ready marker and the complete marker in the same read.
    assert.equal(burst.readIds, 102);

    const writer = await context.newPage();
    await writer.goto(`${origin}/observer-check`);
    await writer.evaluate(openProjection);
    await writer.evaluate(async () =>
      window.putRows([
        {
          id: "entity:agent:0",
          payload: "",
          entity: {
            collection: "agent",
            id: "0",
            value: { id: "0", name: "other tab" },
          },
          seq: 3000,
        },
      ]),
    );
    await page.waitForFunction(
      () =>
        window.snapshots.at(-1)?.threads.find((agent) => agent.id === "0")
          ?.name === "other tab",
    );
    await page.evaluate(async () =>
      window.putRows([{ ...window.entityRow("1", 3001), _deleted: true }]),
    );
    await page.waitForFunction(
      () => window.snapshots.at(-1)?.threads.length === 1999,
    );
    await page.evaluate(async () => {
      window.beforeReset = window.snapshots.length;
      await window.putRows([
        { id: "state:entities:ready", payload: "resetting", seq: 2 },
      ]);
      await window.putRows(
        Array.from({ length: 2000 }, (_, index) => ({
          ...window.entityRow(`${index}`, 0),
          _deleted: true,
        })),
      );
      await window.putRows([window.entityRow("0", 1, "replacement")]);
      await new Promise((resolve) => setTimeout(resolve, 70));
      if (window.snapshots.length !== window.beforeReset)
        throw new Error("Reset published a partial set");
      await window.putRows([
        { id: "state:entities:ready", payload: "ready", seq: 3 },
      ]);
    });
    await page.waitForFunction(
      () => window.snapshots.at(-1)?.threads.length === 1,
    );
    const reset = await page.evaluate(() => ({
      agents: window.snapshots.at(-1)!.threads,
      publications: window.snapshots.length - window.beforeReset,
      queries: window.queryCalls,
    }));
    assert.deepEqual(reset, {
      agents: [{ id: "0", name: "replacement" }],
      publications: 1,
      queries: 2,
    });
    await page.evaluate(() => window.stopObserver());
    assert.deepEqual(errors, []);
    console.log("ENTITY_OBSERVER_BROWSER", JSON.stringify({ burst, reset }));
  } finally {
    await context.close();
    await server.close();
  }
});
