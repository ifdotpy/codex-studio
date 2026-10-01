// Real RxDB/Dexie entity reset, with a hidden Chromium window and isolated API.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium } = require("playwright-core");
const { createServer } = await import(
  new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url)
);
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
});
server.middlewares.use("/sync-reset-check", (_request, response) => {
  response.setHeader("Content-Type", "text/html");
  response.end("<!doctype html><title>Sync reset contract</title>");
});
await server.listen();
const browser = await chromium.launch({
  executablePath:
    process.env.CHROME_BIN ||
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  headless: true,
});
try {
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const workspaceId = "b".repeat(32);
  let releaseSecondPage;
  let secondPageRequested;
  const secondPageWaiting = new Promise((resolve) => {
    secondPageRequested = resolve;
  });
  const secondPageRelease = new Promise((resolve) => {
    releaseSecondPage = resolve;
  });
  const requests = [];
  await page.route("**/sync-reset-check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Sync reset contract</title>",
    }),
  );
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ json: { workspaceId } }),
  );
  await page.route("**/api/sync/stream**", (route) =>
    route.fulfill({
      contentType: "text/event-stream",
      body: ": heartbeat\n\n",
    }),
  );
  await page.route("**/api/sync/drafts", (route) =>
    route.fulfill({ json: [] }),
  );
  await page.route("**/api/sync/pull?**", async (route) => {
    const url = new URL(route.request().url());
    const scope = url.searchParams.get("scope");
    const after = Number(url.searchParams.get("after"));
    requests.push({
      scope,
      after,
      reset: url.searchParams.get("reset"),
      fresh: url.searchParams.get("fresh"),
    });
    if (scope === "drafts")
      return route.fulfill({
        json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
      });
    if (after === 950)
      return route.fulfill({
        json: { workspaceId, reset: true, floor: 1000, maxSeq: 1000 },
      });
    if (after === 0) {
      const documents = Array.from({ length: 500 }, (_, index) => {
        const id = `agent-${index}`;
        return {
          id: `entity:agent:${id}`,
          payload: JSON.stringify({
            collection: "agent",
            id,
            value: { id, name: `new-${index}`, status: "running" },
          }),
          seq: index + 1,
          _deleted: false,
        };
      });
      return route.fulfill({
        json: {
          workspaceId,
          documents,
          checkpoint: { seq: 500 },
          maxSeq: 1000,
          initialHigh: 1000,
        },
      });
    }
    if (after === 500) {
      secondPageRequested();
      await secondPageRelease;
      const id = "agent-500";
      return route.fulfill({
        json: {
          workspaceId,
          documents: [
            {
              id: `entity:agent:${id}`,
              payload: JSON.stringify({
                collection: "agent",
                id,
                value: { id, name: "new-500", status: "running" },
              }),
              seq: 501,
              _deleted: false,
            },
          ],
          checkpoint: { seq: 1000 },
          maxSeq: 1000,
          initialHigh: 1000,
        },
      });
    }
    if (after === 1000)
      return route.fulfill({
        json: { workspaceId, documents: [], checkpoint: { seq: 1000 }, maxSeq: 1000 },
      });
    throw new Error(`Unexpected entity cursor ${after}`);
  });
  await page.goto(
    `http://127.0.0.1:${server.httpServer.address().port}/sync-reset-check`,
  );
  await page.evaluate(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    await db.projections.insert({
      id: "entity:agent:stale",
      payload: JSON.stringify({
        collection: "agent",
        id: "stale",
        value: { id: "stale", name: "stale" },
      }),
      seq: 950,
    });
    await db.projections.insert({
      id: "state:entities:checkpoint",
      payload: "{}",
      seq: 950,
    });
    await db.projections.insert({
      id: "state:entities:ready",
      payload: "ready",
      seq: 1,
    });
    await db.drafts.insert({
      id: "device:lead",
      payload: JSON.stringify({
        device: "device",
        session: "lead",
        text: "keep draft",
      }),
      seq: 1,
    });
    await db.outbox.insert({
      id: "intent-1",
      payload: JSON.stringify({ id: "intent-1", text: "keep outbox" }),
      seq: 1,
    });
    window.entityValues = [];
    window.stopProjection = await (
      await import("/src/sync/client.ts")
    ).watchProjection(
      "state",
      (value) =>
        window.entityValues.push(
          value.runtime.agents.map((agent) => agent.name).sort(),
        ),
      (error) => {
        if (error) window.syncError = error?.stack || String(error);
      },
    );
  });
  let secondPageTimeout;
  await Promise.race([
    secondPageWaiting,
    new Promise((_, reject) => {
      secondPageTimeout = setTimeout(async () => {
        const state = await page.evaluate(async () => {
          const { db } = await (
            await import("/src/sync/client.ts")
          ).syncDatabase();
          const rows = await db.projections.find().exec();
          return {
            error: window.syncError,
            docs: rows.map((doc) => ({
              id: doc.id,
              seq: doc.seq,
              payload: doc.payload,
              deleted: doc._deleted,
            })),
          };
        });
        reject(
          new Error(
            `Second page was not requested: ${JSON.stringify({ requests, state })}`,
          ),
        );
      }, 5000);
    }),
  ]);
  clearTimeout(secondPageTimeout);
  const paused = await page.evaluate(() => window.entityValues);
  assert.ok(
    paused.every((names) => !names.some((name) => name.startsWith("new-"))),
    "the hidden reset must not publish its first partial page",
  );
  releaseSecondPage();
  await page.waitForFunction(
    () => window.entityValues.at(-1)?.length === 501,
    null,
    { timeout: 15000 },
  );
  const result = await page.evaluate(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    const draft = await db.drafts.findOne("device:lead").exec();
    const outbox = await db.outbox.findOne("intent-1").exec();
    return {
      names: window.entityValues.at(-1),
      draft: JSON.parse(draft.payload).text,
      outbox: JSON.parse(outbox.payload).text,
      stale: await db.projections.findOne("entity:agent:stale").exec(),
      checkpoint: (
        await db.projections.findOne("state:entities:checkpoint").exec()
      )?.toJSON().seq,
    };
  });
  assert.equal(result.names.includes("stale"), false);
  assert.equal(result.names.length, 501);
  assert.equal(result.draft, "keep draft");
  assert.equal(result.outbox, "keep outbox");
  assert.equal(result.stale, null);
  assert.equal(result.checkpoint, 1000);
  assert.ok(
    requests.some((request) => request.after === 950 && request.reset === "1"),
  );
  assert.ok(
    requests.some(
      (request) =>
        request.after === 0 && request.fresh === "1" && request.reset === "1",
    ),
    JSON.stringify(requests),
  );
  assert.ok(
    requests.some(
      (request) =>
        request.after === 500 && request.fresh === "1" && request.reset === "1",
    ),
    JSON.stringify(requests),
  );
  assert.deepEqual(errors, []);
  console.log("sync reset browser contract passed");
} finally {
  await browser.close();
  await server.close();
}
