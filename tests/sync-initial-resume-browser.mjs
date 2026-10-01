// A partial first page keeps its original baseline across a browser restart.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium } = require("playwright-core");
const { createServer } = await import(
  new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url)
);
const server = await createServer({ configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 } });
await server.listen();
const browser = await chromium.launch({ headless: true,
  executablePath: process.env.CHROME_BIN ||
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" });
try {
  const page = await browser.newPage();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  const workspaceId = "a".repeat(32);
  const pulls = [];
  await page.route("**/check", (route) => route.fulfill({
    contentType: "text/html", body: "<!doctype html><title>Sync resume</title>" }));
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ json: { workspaceId } }));
  await page.route("**/api/sync/stream**", (route) => route.fulfill({
    contentType: "text/event-stream", body: "data: 601\n\n" }));
  await page.route("**/api/sync/pull?*", (route) => {
    const query = new URL(route.request().url()).searchParams;
    pulls.push(Object.fromEntries(query));
    return route.fulfill({ json: { workspaceId,
      documents: [{ id: "entity:agent:stale", seq: 601, _deleted: true,
        payload: JSON.stringify({ collection: "agent", id: "stale", value: {} }) }],
      checkpoint: { seq: 601 }, maxSeq: 601, initialHigh: 600 } });
  });
  await page.goto(origin + "/check");
  await page.evaluate(async () => {
    const client = await import("/src/sync/client.ts");
    const { db } = await client.syncDatabase();
    await client.persistProjection(db.projections, {
      id: "state:entities:initial", payload: "{}", seq: 600 });
    await client.persistProjection(db.projections, {
      id: "entity:agent:stale", seq: 1,
      payload: JSON.stringify({ collection: "agent", id: "stale",
        value: { id: "stale", name: "Old" } }) });
    window.snapshots = [];
    window.stop = await client.watchProjection("state",
      (value) => window.snapshots.push(value),
      (error) => { if (error) window.syncError = String(error); });
  });
  await page.waitForFunction(() => window.snapshots.length > 0 || window.syncError);
  assert.equal(await page.evaluate(() => window.syncError), undefined);
  assert.deepEqual(pulls[0], { scope: "state:entities:v1", after: "0",
    limit: "500", fresh: "1", initialHigh: "600" });
  assert.equal(await page.evaluate(() => window.snapshots.at(-1).threads.length), 0);
  assert.equal(await page.evaluate(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    const [row] = await db.projections.storageInstance.findDocumentsById(
      ["entity:agent:stale"], true);
    return row._deleted;
  }), true);
  await page.evaluate(() => window.stop());
  console.log("initial sync resume browser passed");
} finally {
  await browser.close();
  await server.close();
}
