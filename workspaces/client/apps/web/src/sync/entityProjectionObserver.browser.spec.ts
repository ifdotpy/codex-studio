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

test("entity observer applies RxDB changes across tabs and resets before publication", async ({
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
      () => window.syncObserver.snapshots.at(-1)?.threads.length === 2000,
    );
    await page.evaluate(async () => {
      window.syncObserver.beforeBurst = window.syncObserver.snapshots.length;
      window.syncObserver.beforeBurstReads =
        window.syncObserver.observerReads.length;
      window.syncObserver.burstStartedAt = performance.now();
      await window.syncObserver.putRows(
        Array.from({ length: 100 }, (_, index) =>
          window.syncObserver.entityRow(
            `${index}`,
            2001 + index,
            `changed-${index}`,
          ),
        ),
      );
    });
    await page.waitForFunction(
      () =>
        window.syncObserver.snapshots
          .at(-1)
          ?.threads.find((agent) => agent.id === "99")?.name === "changed-99",
    );
    const burst = await page.evaluate(() => ({
      publications:
        window.syncObserver.snapshots.length - window.syncObserver.beforeBurst,
      queries: window.syncObserver.queryCalls,
      unchanged:
        window.syncObserver.snapshots[0]?.threads[100] ===
        window.syncObserver.snapshots.at(-1)?.threads[100],
      storageReads:
        window.syncObserver.observerReads.length -
        window.syncObserver.beforeBurstReads,
      readIds: window.syncObserver.observerReads.at(-1)?.length,
      elapsedMs:
        window.syncObserver.lastPublishedAt -
        window.syncObserver.burstStartedAt,
    }));
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
      window.syncObserver.putRows([
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
        window.syncObserver.snapshots
          .at(-1)
          ?.threads.find((agent) => agent.id === "0")?.name === "other tab",
    );
    await page.evaluate(async () =>
      window.syncObserver.putRows([
        { ...window.syncObserver.entityRow("1", 3001), _deleted: true },
      ]),
    );
    await page.waitForFunction(
      () => window.syncObserver.snapshots.at(-1)?.threads.length === 1999,
    );
    await page.evaluate(async () => {
      window.syncObserver.beforeReset = window.syncObserver.snapshots.length;
      window.syncObserver.staleDocuments =
        await window.syncObserver.projections.storageInstance.findDocumentsById(
          ["entity:agent:0", "entity:agent:2"],
          true,
        );
      await window.syncObserver.putRows([
        { id: "state:entities:ready", payload: "resetting", seq: 2 },
      ]);
      [window.syncObserver.staleMarker] =
        await window.syncObserver.projections.storageInstance.findDocumentsById(
          ["state:entities:ready"],
          true,
        );
      await window.syncObserver.putRows(
        Array.from({ length: 2000 }, (_, index) => ({
          ...window.syncObserver.entityRow(`${index}`, 0),
          _deleted: true,
        })),
      );
      await window.syncObserver.putRows([
        window.syncObserver.entityRow("0", 1, "replacement"),
      ]);
      await new Promise((resolve) => setTimeout(resolve, 70));
      if (
        window.syncObserver.snapshots.length !== window.syncObserver.beforeReset
      )
        throw new Error("Reset published a partial set");
      await window.syncObserver.putRows([
        { id: "state:entities:ready", payload: "ready", seq: 3 },
      ]);
    });
    await page.waitForFunction(
      () => window.syncObserver.snapshots.at(-1)?.threads.length === 1,
    );
    const reset = await page.evaluate(() => ({
      agents: window.syncObserver.snapshots.at(-1)?.threads,
      publications:
        window.syncObserver.snapshots.length - window.syncObserver.beforeReset,
      queries: window.syncObserver.queryCalls,
    }));
    assert.deepEqual(reset, {
      agents: [{ id: "0", name: "replacement" }],
      publications: 1,
      queries: 2,
    });
    const afterStaleEvents = await page.evaluate(async () => {
      if (!window.syncObserver.staleMarker)
        throw new Error("Missing stale marker");
      const collection = window.syncObserver.projections;
      const database = collection.database;
      database.$emit({
        id: "delayed-old-epoch-events",
        collectionName: collection.name,
        isLocal: false,
        internal: false,
        databaseToken: database.token,
        storageToken: await database.storageToken,
        checkpoint: { id: "", lwt: 0 },
        context: "observer-stale-event-check",
        events: [
          ...window.syncObserver.staleDocuments,
          window.syncObserver.staleMarker,
        ].map((documentData) => ({
          operation: "UPDATE",
          documentId: documentData.id,
          documentData,
          previousDocumentData: documentData,
        })),
      });
      await new Promise((resolve) => setTimeout(resolve, 70));
      return {
        agents: window.syncObserver.snapshots.at(-1)?.threads,
        publications:
          window.syncObserver.snapshots.length -
          window.syncObserver.beforeReset,
        queries: window.syncObserver.queryCalls,
      };
    });
    assert.deepEqual(afterStaleEvents, reset);
    await page.evaluate(() => window.syncObserver.stopObserver());
    assert.deepEqual(errors, []);
    console.log("ENTITY_OBSERVER_BROWSER", JSON.stringify({ burst, reset }));
  } finally {
    await context.close();
    await server.close();
  }
});
