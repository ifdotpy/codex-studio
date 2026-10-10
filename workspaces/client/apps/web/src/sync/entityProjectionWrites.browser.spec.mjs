import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  expect,
  protocol3SseEvent,
  readApiSchemaHash,
  test,
} from "../../tests/playwright.mjs";

for (const writerTab of [0, 1]) {
  test(`projection reset fences an accepted write from tab ${writerTab + 1}`, async ({
    browser,
  }) => {
    test.setTimeout(45_000);
    const { createServer } =
      await import("../../node_modules/vite/dist/node/index.js");
    const workspaceId = "e".repeat(32);
    const streams = new Set();
    const rows = new Map();
    let sequence = 2;
    let revision = 0;
    let resetPending = false;
    const row = (id, name, seq) => ({
      id: `entity:agent:${id}`,
      seq,
      payload: JSON.stringify({
        collection: "agent",
        id,
        value: { id, name, status: "running", source: "managed" },
      }),
      _deleted: false,
    });
    rows.set("lead", row("lead", "Lead", 1));
    rows.set("existing", row("existing", "Existing", 2));
    const frame = (reason, changes, reset = false) => ({
      protocol: 3,
      workspaceId,
      epoch: "projection-reset-race",
      revision: ++revision,
      reason,
      resources: [{ kind: "state" }],
      resourceVersions: [
        {
          revision: sequence,
          ...(changes
            ? {
                entitySequences: changes.documents.map(
                  (document) => document.seq,
                ),
                entityChanges: changes,
              }
            : {}),
          ...(reset ? { entitySequenceReset: true } : {}),
        },
      ],
    });
    const send = (event) => {
      for (const response of streams)
        response.write(protocol3SseEvent("resources", event));
    };
    const cacheDir = await mkdtemp(
      join(tmpdir(), "studio-entity-writes-vite-"),
    );
    const server = await createServer({
      configFile: false,
      cacheDir,
      root: fileURLToPath(new URL("../..", import.meta.url)),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "entity-projection-reset-race",
          configureServer(server) {
            server.middlewares.use((request, response, next) => {
              const url = new URL(request.url, "http://localhost");
              if (url.pathname === "/projection-reset-check") {
                response.setHeader("Content-Type", "text/html");
                response.end(
                  "<!doctype html><title>Projection reset check</title>",
                );
                return;
              }
              if (url.pathname === "/api/sync/identity") {
                response.setHeader("Content-Type", "application/json");
                response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
                response.end(JSON.stringify({ workspaceId }));
                return;
              }
              if (url.pathname === "/api/sync/stream") {
                response.setHeader("Content-Type", "text/event-stream");
                response.write(apiSchemaHandshakeSse());
                response.write(
                  protocol3SseEvent("resources", frame("initial")),
                );
                streams.add(response);
                response.on("close", () => streams.delete(response));
                return;
              }
              if (url.pathname === "/api/sync/pull") {
                const after = Number(url.searchParams.get("after"));
                response.setHeader("Content-Type", "application/json");
                response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
                if (resetPending && after > 0) {
                  resetPending = false;
                  response.end(
                    JSON.stringify({
                      workspaceId,
                      reset: true,
                      floor: 4,
                      maxSeq: sequence,
                    }),
                  );
                } else {
                  response.end(
                    JSON.stringify({
                      workspaceId,
                      documents: [...rows.values()]
                        .filter((document) => document.seq > after)
                        .sort((left, right) => left.seq - right.seq),
                      checkpoint: { seq: sequence },
                      initialHigh: Number(
                        url.searchParams.get("initialHigh") || 0,
                      ),
                      maxSeq: sequence,
                    }),
                  );
                }
                return;
              }
              next();
            });
          },
        },
      ],
    });
    const context = await browser.newContext();
    const pages = [];
    const errors = [];
    const openPage = async () => {
      const page = await context.newPage();
      pages.push(page);
      page.on("pageerror", (error) => errors.push(error.message));
      await page.goto(
        `http://127.0.0.1:${server.httpServer.address().port}/projection-reset-check`,
      );
      await page.evaluate(async () => {
        const client = await import("/src/sync/client.ts");
        window.client = client;
        window.snapshot = null;
        window.errors = [];
        window.connection = "closed";
        client.watchResourceConnection((state) => (window.connection = state));
        client.subscribeStateProjection(
          (snapshot) => (window.snapshot = snapshot),
          (error) => {
            if (error) window.errors.push(String(error));
          },
        );
        const { db } = await client.syncDatabase();
        window.db = db;
        window.checkpoint = async () =>
          (
            await db.projections.storageInstance.findDocumentsById(
              ["state:entities:checkpoint"],
              true,
            )
          )[0]?.seq;
      });
      await page.waitForFunction(
        () =>
          window.snapshot?.runtime.agents.some(
            (agent) => agent.id === "lead",
          ) && window.connection === "live",
      );
      return page;
    };
    try {
      await server.listen();
      const primary = await openPage();
      if (writerTab) await openPage();
      await expect.poll(() => streams.size).toBe(1);
      const writer = pages[writerTab];
      await writer.evaluate(() => {
        const storage = window.db.projections.storageInstance;
        const original = storage.bulkWrite.bind(storage);
        storage.bulkWrite = async (writes, ...rest) => {
          if (
            !window.writeStarted &&
            writes.some(
              (entry) =>
                entry.document.id === "entity:agent:ghost" &&
                entry.document.seq === 3,
            )
          ) {
            window.writeStarted = true;
            await new Promise((resolve) => (window.releaseWrite = resolve));
          }
          return original(writes, ...rest);
        };
      });
      sequence = 3;
      const accepted = frame("change", {
        after: 2,
        through: 3,
        documents: [row("ghost", "Already deleted", 3)],
      });
      if (writerTab) {
        await writer.evaluate((event) => {
          window.write = window.client.persistResourceEntityChanges(
            event,
            () => true,
          );
        }, accepted);
      } else {
        send(accepted);
      }
      await writer.waitForFunction(() => window.writeStarted);
      sequence = 4;
      rows.set("fresh", row("fresh", "Replacement baseline", 4));
      resetPending = true;
      send(frame("overflow", null, true));
      await primary.waitForTimeout(150);
      assert.equal(
        await primary.evaluate(() =>
          window.snapshot?.runtime.agents.some((agent) => agent.id === "fresh"),
        ),
        false,
        "Reset must wait for the accepted storage write",
      );
      await writer.evaluate(() => window.releaseWrite());
      await Promise.all(
        pages.map((page) =>
          page.waitForFunction(
            async () =>
              window.snapshot?.runtime.agents.some(
                (agent) => agent.id === "fresh",
              ) && (await window.checkpoint()) === 4,
          ),
        ),
      );
      await primary.waitForTimeout(150);
      for (const page of pages) {
        assert.equal(
          await page.evaluate(() =>
            window.snapshot.runtime.agents.some(
              (agent) => agent.id === "ghost",
            ),
          ),
          false,
          "Reset must delete the old insert after its storage write finishes",
        );
      }
      send(accepted);
      await primary.waitForTimeout(150);
      for (const page of pages) {
        assert.equal(
          await page.evaluate(() =>
            window.snapshot.runtime.agents.some(
              (agent) => agent.id === "ghost",
            ),
          ),
          false,
          "A stale peer frame must not resurrect a row removed by reset",
        );
        assert.deepEqual(await page.evaluate(() => window.errors), []);
      }
      assert.deepEqual(errors, []);
    } finally {
      try {
        await context.close();
        for (const response of streams) response.end();
        await server.close();
      } finally {
        await rm(cacheDir, { recursive: true, force: true });
      }
    }
  });
}
