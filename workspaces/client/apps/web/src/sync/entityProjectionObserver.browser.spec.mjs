import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  test,
} from "../../tests/playwright.mjs";

async function openProjection() {
  const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
  window.projections = db.projections;
  window.putRows = async (rows) => {
    const storage = db.projections.storageInstance;
    const previous = new Map(
      (
        await storage.findDocumentsById(
          rows.map((row) => row.id),
          true,
        )
      ).map((row) => [row.id, row]),
    );
    const result = await storage.bulkWrite(
      rows.map((row) => ({
        previous: previous.get(row.id),
        document: {
          ...row,
          _deleted: row._deleted ?? false,
          _attachments: {},
          _meta: { lwt: 1 },
          _rev: "",
        },
      })),
      "observer-browser-check",
    );
    if (result.error.length) throw new Error(JSON.stringify(result.error));
  };
}

test("entity observer applies RxDB changes across tabs and resets before publication", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  const { createServer } =
    await import("../../node_modules/vite/dist/node/index.js");
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
        json: { workspaceId: "a".repeat(32) },
        headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
      }),
    );
    const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
    const page = await context.newPage();
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(`${origin}/observer-check`);
    await page.evaluate(openProjection);
    await page.evaluate(async () => {
      window.entityRow = (id, seq, name = id) => ({
        id: `entity:agent:${id}`,
        seq,
        payload: JSON.stringify({
          collection: "agent",
          id,
          value: { id, name },
        }),
      });
      await window.putRows([
        ...Array.from({ length: 2000 }, (_, index) =>
          window.entityRow(`${index}`, index + 1),
        ),
        { id: "state:entities:ready", payload: "ready", seq: 1 },
      ]);
      window.queryCalls = 0;
      window.observerReads = [];
      const read = window.projections.storageInstance.findDocumentsById.bind(
        window.projections.storageInstance,
      );
      window.projections.storageInstance.findDocumentsById = (ids, ...args) => {
        if (ids.includes("state:entities:ready"))
          window.observerReads.push(ids);
        return read(ids, ...args);
      };
      const find = window.projections.find.bind(window.projections);
      window.projections.find = (...args) => {
        window.queryCalls++;
        return find(...args);
      };
      window.snapshots = [];
      window.stopObserver = await (
        await import("/src/sync/entityProjectionObserver.ts")
      ).observeEntityProjection(
        window.projections,
        (value) => {
          window.snapshots.push(value);
          window.lastPublishedAt = performance.now();
        },
        (error) => {
          throw error;
        },
      );
    });
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
    const burst = await page.evaluate(() => ({
      publications: window.snapshots.length - window.beforeBurst,
      queries: window.queryCalls,
      unchanged:
        window.snapshots[0].threads[100] ===
        window.snapshots.at(-1).threads[100],
      storageReads: window.observerReads.length - window.beforeBurstReads,
      readIds: window.observerReads.at(-1).length,
      elapsedMs: window.lastPublishedAt - window.burstStartedAt,
    }));
    assert.equal(burst.publications, 1);
    assert.equal(burst.queries, 1, JSON.stringify(burst));
    assert.equal(burst.unchanged, true);
    assert.equal(burst.storageReads, 1);
    assert.equal(burst.readIds, 101);

    const writer = await context.newPage();
    await writer.goto(`${origin}/observer-check`);
    await writer.evaluate(openProjection);
    await writer.evaluate(async () =>
      window.putRows([
        {
          id: "entity:agent:0",
          payload: JSON.stringify({
            collection: "agent",
            id: "0",
            value: { id: "0", name: "other tab" },
          }),
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
      window.staleDocuments =
        await window.projections.storageInstance.findDocumentsById(
          ["entity:agent:0", "entity:agent:2"],
          true,
        );
      await window.putRows([
        { id: "state:entities:ready", payload: "resetting", seq: 2 },
      ]);
      [window.staleMarker] =
        await window.projections.storageInstance.findDocumentsById(
          ["state:entities:ready"],
          true,
        );
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
      agents: window.snapshots.at(-1).threads,
      publications: window.snapshots.length - window.beforeReset,
      queries: window.queryCalls,
    }));
    assert.deepEqual(reset, {
      agents: [{ id: "0", name: "replacement" }],
      publications: 1,
      queries: 2,
    });
    const afterStaleEvents = await page.evaluate(async () => {
      const collection = window.projections;
      const database = collection.database;
      database.$emit({
        id: "delayed-old-epoch-events",
        collectionName: collection.name,
        isLocal: false,
        internal: false,
        databaseToken: database.token,
        storageToken: await database.storageToken,
        checkpoint: {},
        context: "observer-stale-event-check",
        events: [...window.staleDocuments, window.staleMarker].map(
          (documentData) => ({
            operation: "UPDATE",
            documentId: documentData.id,
            documentData,
            previousDocumentData: documentData,
          }),
        ),
      });
      await new Promise((resolve) => setTimeout(resolve, 70));
      return {
        agents: window.snapshots.at(-1).threads,
        publications: window.snapshots.length - window.beforeReset,
        queries: window.queryCalls,
      };
    });
    assert.deepEqual(afterStaleEvents, reset);
    const transcript = await page.evaluate(async () => {
      const cache = await import("/src/sync/transcriptCache.ts");
      cache.cacheTranscriptValue(
        "workspace",
        "chat",
        { items: [{ id: "a", text: "initial" }] },
        1,
      );
      const values = [];
      const stop = cache.subscribeTranscript("workspace", "chat", (value) =>
        values.push(value),
      );
      for (let index = 2; index <= 101; index++)
        cache.patchTranscriptValue(
          "workspace",
          "chat",
          {},
          index,
          [{ id: "a", text: `${index}` }],
          [],
        );
      cache.patchTranscriptValue(
        "workspace",
        "chat",
        {},
        102,
        [{ id: "b", text: "tool", toolStatus: "running" }],
        [],
      );
      cache.patchTranscriptValue("workspace", "chat", {}, 103, [], ["a"]);
      cache.patchTranscriptValue(
        "workspace",
        "chat",
        {},
        104,
        [{ id: "b", text: "done", toolStatus: "completed" }],
        [],
      );
      const immediateSequence = cache.peekTranscript("workspace", "chat").seq;
      await new Promise((resolve) => setTimeout(resolve, 70));
      stop();
      window.stopObserver();
      return {
        immediateSequence,
        publications: values.length,
        items: values.at(-1).payload.items,
      };
    });
    assert.deepEqual(transcript, {
      immediateSequence: 104,
      publications: 2,
      items: [{ id: "b", text: "done", toolStatus: "completed" }],
    });
    assert.deepEqual(errors, []);
    console.log(
      "ENTITY_OBSERVER_BROWSER",
      JSON.stringify({ burst, reset, transcript }),
    );
  } finally {
    await context.close();
    await server.close();
  }
});
