// Real RxDB/Dexie applies a resumable change event received by the renderer.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium } = require("playwright-core");
const { createServer } = await import(new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url));
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
});
server.middlewares.use("/push-check", (_request, response) => {
  response.setHeader("Content-Type", "text/html");
  response.end("<!doctype html><title>Sync push check</title>");
});
await server.listen();
const browser = await chromium.launch({
  executablePath: process.env.CHROME_BIN || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  headless: true,
});
try {
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const workspaceId = "c".repeat(32);
  const frame = {
    protocolVersion: 1,
    workspaceId,
    scope: "state:entities:v1",
    documents: [{
      id: "entity:agent:lead",
      payload: JSON.stringify({ collection: "agent", id: "lead", value: { id: "lead", name: "Pushed lead", status: "running" } }),
      seq: 1,
      _deleted: false,
    }],
    cursor: 1,
  };
  const streams = [];
  await page.route("**/push-check", (route) => route.fulfill({ contentType: "text/html", body: "<!doctype html>" }));
  await page.route("**/api/session", (route) => route.fulfill({ json: { token: "fixture" } }));
  await page.route("**/api/sync/identity", (route) => route.fulfill({ json: { workspaceId } }));
  await page.route("**/api/sync/protocol", (route) => route.fulfill({ json: { protocolVersion: 1, capabilities: ["pull", "streamChanges"] } }));
  await page.route("**/api/sync/stream**", (route) => {
    const url = new URL(route.request().url());
    streams.push(url);
    return route.fulfill({
      contentType: "text/event-stream",
      body: `id: 1\nevent: changes\ndata: ${JSON.stringify(frame)}\n\n: heartbeat\n\n`,
    });
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/push-check`);
  await page.evaluate(async () => {
    const client = await import("/src/sync/client.ts");
    const { db } = await client.syncDatabase();
    window.pushState = undefined;
    window.stopPush = client.watchSyncInvalidations(async () => {
      const row = await db.projections.findOne("entity:agent:lead").exec();
      window.pushState = row && JSON.parse(row.payload).value.name;
    }, "entities");
  });
  await page.waitForFunction(() => window.pushState === "Pushed lead");
  assert.equal(streams.length, 1);
  assert.equal(streams[0].searchParams.get("protocol"), "1");
  assert.equal(streams[0].searchParams.get("scope"), "state:entities:v1");
  assert.equal(errors.length, 0, errors.join("; "));
  await page.evaluate(() => window.stopPush());
  console.log("sync push browser contract passed");
} finally {
  await browser.close();
  await server.close();
}
