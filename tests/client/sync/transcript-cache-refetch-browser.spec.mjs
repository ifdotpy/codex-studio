import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import { test } from "../playwright.mjs";

test("legacy transcript rows refetch online and stay hidden offline", async ({
  browser,
}) => {
  test.setTimeout(120_000);
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
  server.middlewares.use("/transcript-cache-check", (_request, response) => {
    response.setHeader("Content-Type", "text/html");
    response.end("<!doctype html><title>Transcript cache recovery</title>");
  });
  await server.listen();
  const context = await browser.newContext();
  try {
    const page = await context.newPage();
    let workspaceId = "e".repeat(32);
    let responseMode = "unchanged";
    let deltaReplies = 0;
    const pulls = [];
    await context.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: { workspaceId } }),
    );
    await context.route("**/api/sync/pull?**", (route) => {
      const url = new URL(route.request().url());
      const scope = url.searchParams.get("scope");
      const after = Number(url.searchParams.get("after"));
      pulls.push({ scope, after });
      if (scope === "drafts")
        return route.fulfill({
          json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
        });
      if (scope !== "transcript:lead")
        throw new Error(`Unexpected sync scope ${scope}`);
      if (responseMode === "delta" && after === 0 && deltaReplies++ === 0)
        return route.fulfill({
          json: {
            workspaceId,
            documents: [
              {
                id: scope,
                seq: 2,
                payload: JSON.stringify({
                  delta: true,
                  items: [{ id: "item-a", text: "delta without a local base" }],
                }),
              },
            ],
            checkpoint: { seq: 2 },
          },
        });
      const snapshot = {
        title: "Unchanged chat",
        items: [{ id: "item-a", text: "still here" }],
      };
      return route.fulfill({
        json: {
          workspaceId,
          documents:
            after === 0
              ? [{ id: scope, seq: 1, payload: JSON.stringify(snapshot) }]
              : [],
          checkpoint: { seq: after === 0 ? 1 : after },
        },
      });
    });
    const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
    await page.goto(`${origin}/transcript-cache-check`);
    const online = await page.evaluate(async (workspaceId) => {
      const { syncDatabase, prefetchTranscript } =
        await import("/src/sync/client.ts");
      const { peekTranscript } = await import("/src/sync/transcriptCache.ts");
      const { db } = await syncDatabase();
      await db.projections.insert({
        id: "transcript:lead",
        seq: 9,
        payload: JSON.stringify({
          items: [{ id: "cached", text: "old cache" }],
        }),
      });
      const succeeded = await prefetchTranscript(workspaceId, "lead");
      const row = await db.projections.findOne("transcript:lead").exec();
      return {
        succeeded,
        payload: peekTranscript(workspaceId, "lead")?.payload,
        stored: JSON.parse(row.payload),
      };
    }, workspaceId);
    assert.equal(online.succeeded, true);
    assert.deepEqual(online.payload.items, [
      { id: "item-a", text: "still here" },
    ]);
    assert.equal(online.stored.format, 2);
    assert.deepEqual(online.stored.order, ["item-a"]);
    assert.ok(
      pulls.some(
        (pull) => pull.scope === "transcript:lead" && pull.after === 0,
      ),
    );

    responseMode = "delta";
    workspaceId = "d".repeat(32);
    deltaReplies = 0;
    const deltaPage = await context.newPage();
    await deltaPage.goto(`${origin}/transcript-cache-check`);
    const delta = await deltaPage.evaluate(async (workspaceId) => {
      const { syncDatabase, prefetchTranscript } =
        await import("/src/sync/client.ts");
      const { db } = await syncDatabase();
      const succeeded = await prefetchTranscript(workspaceId, "lead");
      const row = await db.projections.findOne("transcript:lead").exec();
      return { succeeded, stored: JSON.parse(row.payload) };
    }, workspaceId);
    assert.equal(delta.succeeded, true);
    assert.equal(delta.stored.format, 2);
    assert.deepEqual(delta.stored.order, ["item-a"]);
    assert.ok(
      pulls.filter(
        (pull) => pull.scope === "transcript:lead" && pull.after === 0,
      ).length >= 3,
      "A transcript delta without an item base triggers a full pull",
    );

    const offlinePage = await context.newPage();
    responseMode = "unchanged";
    workspaceId = "f".repeat(32);
    await offlinePage.goto(`${origin}/transcript-cache-check`);
    await offlinePage.evaluate(() => {
      Object.defineProperty(navigator, "onLine", {
        configurable: true,
        value: false,
      });
      window.pageErrors = [];
      window.addEventListener("error", (event) =>
        window.pageErrors.push(event.message),
      );
    });
    const offline = await offlinePage.evaluate(async () => {
      const { prefetchTranscript } = await import("/src/sync/client.ts");
      const { peekTranscript } = await import("/src/sync/transcriptCache.ts");
      const workspaceId = "f".repeat(32);
      return {
        refreshed: await prefetchTranscript(workspaceId, "lead"),
        cached: peekTranscript(workspaceId, "lead") ?? null,
        errors: window.pageErrors,
      };
    });
    assert.deepEqual(offline, { refreshed: false, cached: null, errors: [] });
  } finally {
    await context.close();
    await server.close();
  }
});
