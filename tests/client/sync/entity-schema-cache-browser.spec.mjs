import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import { test } from "../playwright.mjs";

test("different build collection sets share only drafts and outbox", async ({
  browser,
}) => {
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
  server.middlewares.stack.unshift({
    route: "",
    handle(request, response, next) {
      if (
        new URL(request.url || "/", "http://localhost").pathname !==
        "/sync-check"
      )
        return next();
      response.setHeader("Content-Type", "text/html");
      response.end("<!doctype html><title>Cache collection sets</title>");
    },
  });
  await server.listen();
  const context = await browser.newContext();
  const oldPage = await context.newPage();
  const newPage = await context.newPage();
  const databaseName = `studio-schema-cache-${crypto.randomUUID().replaceAll("-", "")}`;
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  try {
    await Promise.all([
      oldPage.goto(`${origin}/sync-check`),
      newPage.goto(`${origin}/sync-check`),
    ]);
    await oldPage.evaluate(async (name) => {
      const fixture = await import("/src/sync/schemaCacheBrowserFixture.ts");
      const db = await fixture.openBuildDatabase(name, true);
      await db.projections.insert({
        id: "entity:old-build",
        payload: "old-shape",
        seq: 1,
      });
      await db.projections.insert({
        id: "transcript:old-build-agent",
        payload: JSON.stringify([{ role: "assistant", content: "legacy" }]),
        seq: 2,
      });
      await db.drafts.insert({
        id: "draft-from-old",
        payload: "draft",
        seq: 1,
      });
      await db.outbox.insert({
        id: "message-from-old",
        payload: "queued",
        seq: 1,
      });
      window.oldBuildDatabase = db;
    }, databaseName);
    await newPage.evaluate(async (name) => {
      const fixture = await import("/src/sync/schemaCacheBrowserFixture.ts");
      const db = await fixture.openBuildDatabase(name, false);
      if (db.collections.projections)
        throw new Error(
          "The new stable database opened the legacy projection collection",
        );
      const draft = await db.drafts.findOne("draft-from-old").exec();
      const message = await db.outbox.findOne("message-from-old").exec();
      if (draft?.payload !== "draft" || message?.payload !== "queued")
        throw new Error("The new build could not read old user data");
      await db.drafts.insert({
        id: "draft-from-new",
        payload: "new draft",
        seq: 2,
      });
      await db.outbox.insert({
        id: "message-from-new",
        payload: "new queued",
        seq: 2,
      });
      const projections = await fixture.openProjectionDatabase(
        `${name}-entities-hash-a`,
      );
      await projections.projections.insert({
        id: "entity:new-build",
        payload: "new-shape",
        seq: 1,
      });
      const otherHash = await fixture.openProjectionDatabase(
        `${name}-entities-hash-b`,
      );
      if (await otherHash.projections.findOne("entity:new-build").exec())
        throw new Error("Different build caches were shared");
      window.newBuildDatabase = db;
    }, databaseName);
    const oldData = await oldPage.evaluate(async () => ({
      draft: (
        await window.oldBuildDatabase.drafts.findOne("draft-from-new").exec()
      )?.payload,
      message: (
        await window.oldBuildDatabase.outbox.findOne("message-from-new").exec()
      )?.payload,
      projection: (
        await window.oldBuildDatabase.projections
          .findOne("entity:old-build")
          .exec()
      )?.payload,
      legacyProjectionStats: await (async () => {
        const rows = await window.oldBuildDatabase.projections.find().exec();
        const serialized = rows.map(({ id, payload, seq }) => ({
          id,
          payload,
          seq,
        }));
        return {
          rows: serialized.length,
          bytes: new TextEncoder().encode(JSON.stringify(serialized)).length,
        };
      })(),
    }));
    const { legacyProjectionStats, ...sharedData } = oldData;
    assert.deepEqual(sharedData, {
      draft: "new draft",
      message: "new queued",
      projection: "old-shape",
    });
    assert.equal(legacyProjectionStats.rows, 2);
    console.log("Legacy projections fixture:", legacyProjectionStats);
  } finally {
    await context.close();
    await server.close();
  }
});
