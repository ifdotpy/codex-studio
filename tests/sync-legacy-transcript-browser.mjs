// A persisted pre-item transcript must survive the first item delta and reload.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium } = require("playwright-core");
const { createServer } = await import(
  new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url)
);
const workspaceId = "c".repeat(32);
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
});
server.middlewares.use("/transcript-check", (_request, response) => {
  response.setHeader("Content-Type", "text/html");
  response.end("<!doctype html><title>Legacy transcript sync contract</title>");
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
  await page.route("**/transcript-check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Legacy transcript sync contract</title>",
    }),
  );
  let deltaCalls = 0;
  const pulls = [];
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ json: { workspaceId } }),
  );
  await page.route("**/api/sync/protocol", (route) =>
    route.fulfill({ json: { protocolVersion: 1, capabilities: [] } }),
  );
  await page.route("**/api/sync/stream**", (route) =>
    route.fulfill({
      contentType: "text/event-stream",
      body: ": heartbeat\n\n",
    }),
  );
  await page.route("**/api/sync/pull?**", (route) => {
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
    if (after === 1 && deltaCalls++ === 0)
      return route.fulfill({
        json: {
          workspaceId,
          documents: [
            {
              id: scope,
              seq: 2,
              payload: JSON.stringify({
                delta: true,
                title: "Legacy chat",
                items: [{ id: "changed", text: "updated item" }],
                order: ["unchanged", "changed"],
                itemRevisions: { changed: "r2" },
              }),
            },
          ],
          checkpoint: { seq: 2 },
        },
      });
    if (after === 0)
      return route.fulfill({
        json: {
          workspaceId,
          documents: [
            {
              id: scope,
              seq: 2,
              payload: JSON.stringify({
                title: "Legacy chat",
                items: [
                  { id: "unchanged", text: "original item" },
                  { id: "changed", text: "updated item" },
                ],
                order: ["unchanged", "changed"],
              }),
            },
          ],
          checkpoint: { seq: 2 },
        },
      });
    return route.fulfill({
      json: { workspaceId, documents: [], checkpoint: { seq: after } },
    });
  });

  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  await page.goto(`${origin}/transcript-check`);
  await page.evaluate(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    await db.projections.insert({
      id: "transcript:lead",
      seq: 1,
      payload: JSON.stringify({
        title: "Legacy chat",
        items: [
          { id: "unchanged", text: "original item" },
          { id: "changed", text: "old item" },
        ],
        order: ["unchanged", "changed"],
      }),
    });
    window.firstValues = [];
    window.stopTranscript = await (
      await import("/src/sync/client.ts")
    ).watchProjection(
      "transcript:lead",
      (value) => window.firstValues.push(value),
      (error) => {
        if (error) window.syncError = String(error);
      },
    );
  });
  await page.waitForFunction(
    () =>
      window.firstValues.some((value) =>
        value?.items?.some(
          (item) => item.id === "changed" && item.text === "updated item",
        ),
      ),
    null,
    { timeout: 10000 },
  );
  assert.equal(await page.evaluate(() => window.syncError), undefined);
  assert.equal(deltaCalls, 1, "the server delta was applied before reload");
  assert.ok(
    pulls.some(
      ({ scope, after }) => scope === "transcript:lead" && after === 0,
    ),
    "a legacy cache requires a full transcript pull before applying the delta",
  );

  await page.reload();
  await page.evaluate(async () => {
    window.reloadedValues = [];
    window.stopReloadedTranscript = await (
      await import("/src/sync/client.ts")
    ).watchProjection(
      "transcript:lead",
      (value) => window.reloadedValues.push(value),
      (error) => {
        if (error) window.reloadError = String(error);
      },
    );
  });
  await page.waitForFunction(
    () => window.reloadedValues.some((value) => Array.isArray(value?.items)),
    null,
    { timeout: 10000 },
  );
  const transcript = await page.evaluate(() =>
    window.reloadedValues.find((value) => Array.isArray(value?.items)),
  );
  assert.equal(await page.evaluate(() => window.reloadError), undefined);
  assert.deepEqual(
    transcript.items.map((item) => [item.id, item.text]),
    [
      ["unchanged", "original item"],
      ["changed", "updated item"],
    ],
    `Reloaded transcript lost an item; pulls=${JSON.stringify(pulls)} errors=${JSON.stringify(errors)}`,
  );
  assert.deepEqual(errors, []);
  console.log("legacy transcript delta survives reload");
} finally {
  await browser.close();
  await server.close();
}
