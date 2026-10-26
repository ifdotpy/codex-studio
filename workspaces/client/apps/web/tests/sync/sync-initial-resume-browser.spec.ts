import { fixtureOrigin } from "./helpers.ts";
// A saved partial first-page marker reconstructs its original baseline.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { apiSchemaHandshakeSse, test } from "../playwright.ts";

test("sync initial resume browser", async ({ page }) => {
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
    const origin = fixtureOrigin(server);
    const workspaceId = "a".repeat(32);
    const pulls: Record<string, string>[] = [];
    const live = Array.from({ length: 499 }, (_, index) => ({
      op: "upsert",
      collection: "agent",
      id: `worker-${index}`,
      value: { id: `worker-${index}`, name: `Worker ${index}` },
      seq: index + 2,
    }));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Sync resume</title>",
      }),
    );
    await page.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: { workspaceId } }),
    );
    await page.route("**/api/sync/stream**", (route) =>
      route.fulfill({
        contentType: "text/event-stream",
        body: apiSchemaHandshakeSse(),
      }),
    );
    await page.route("**/api/sync/pull?*", (route) => {
      const query = new URL(route.request().url()).searchParams;
      pulls.push(Object.fromEntries(query));
      return route.fulfill({
        json: {
          workspaceId,
          documents: [
            ...live,
            {
              op: "delete",
              collection: "agent",
              id: "stale",
              value: {},
              seq: 601,
            },
          ],
          checkpoint: { seq: 601 },
          maxSeq: 601,
          initialHigh: 600,
        },
      });
    });
    await page.goto(origin + "/check");
    await page.evaluate(async () => {
      const client = await import("/src/sync/client.ts");
      const { db } = await client.syncDatabase();
      await client.persistProjection(db.projections, {
        id: "state:entities:initial",
        payload: "{}",
        seq: 600,
      });
      await client.persistProjection(db.projections, {
        // @ts-expect-error stale seed: persistProjection expects EntityRow (id, payload, seq), not a wire SyncEntity
        op: "upsert",
        collection: "agent",
        id: "stale",
        value: { id: "stale", name: "Old" },
        seq: 1,
      });
      window.syncSyncInitialResumeBrowserWriteCalls = 0;
      const storage = db.projections.storageInstance;
      const write = storage.bulkWrite.bind(storage);
      storage.bulkWrite = (...args) => {
        window.syncSyncInitialResumeBrowserWriteCalls++;
        return write(...args);
      };
      window.syncSyncInitialResumeBrowserSnapshots = [];
      window.syncSyncInitialResumeBrowserStop = client.subscribeStateProjection(
        (value) => window.syncSyncInitialResumeBrowserSnapshots.push(value),
        (error) => {
          if (error)
            window.syncSyncInitialResumeBrowserSyncError = String(error);
        },
      );
    });
    await page.waitForFunction(
      () =>
        window.syncSyncInitialResumeBrowserSnapshots.length > 0 ||
        window.syncSyncInitialResumeBrowserSyncError,
    );
    assert.equal(
      await page.evaluate(() => window.syncSyncInitialResumeBrowserSyncError),
      undefined,
    );
    assert.deepEqual(pulls[0], {
      scope: "state:entities:v1",
      after: "0",
      limit: "500",
      fresh: "1",
      initialHigh: "600",
      reset: "1",
    });
    assert.equal(
      await page.evaluate(
        () =>
          window.syncSyncInitialResumeBrowserSnapshots.at(-1)?.threads.length,
      ),
      499,
    );
    assert.ok(
      await page.evaluate(
        () => window.syncSyncInitialResumeBrowserWriteCalls <= 5,
      ),
      "A 500-row page uses one storage batch plus checkpoint writes",
    );
    assert.equal(
      await page.evaluate(async () => {
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        const [row] = await db.projections.storageInstance.findDocumentsById(
          ["entity:agent:stale"],
          true,
        );
        if (!row) throw new Error("Missing stale projection");
        return row._deleted;
      }),
      true,
    );
    await page.evaluate(() => window.syncSyncInitialResumeBrowserStop());
    console.log("initial sync resume browser passed");
  } finally {
    await server.close();
  }
});

declare global {
  interface Window {
    syncSyncInitialResumeBrowserWriteCalls: number;
    syncSyncInitialResumeBrowserSnapshots: Parameters<
      Parameters<
        typeof import("../../src/sync/client.ts").subscribeStateProjection
      >[0]
    >[0][];
    syncSyncInitialResumeBrowserSyncError: string | undefined;
    syncSyncInitialResumeBrowserStop: () => void;
  }
}
