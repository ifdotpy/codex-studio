// Real RxDB/Dexie reconciles a shared stream invalidation through HTTP pull.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium } = require("playwright-core");
const { createServer } = await import(
  new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url)
);
const workspaceId = "c".repeat(32);
const streams = new Set();
let revision = 1;
let pulls = 0;
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
});
server.middlewares.stack.unshift({
  route: "",
  handle(req, res, next) {
    const url = new URL(req.url, "http://localhost");
    const json = (value) => {
      res.setHeader("Content-Type", "application/json");
      res.end(JSON.stringify(value));
    };
    if (url.pathname === "/push-check") res.end("<!doctype html>");
    else if (url.pathname === "/api/sync/identity") json({ workspaceId });
    else if (url.pathname === "/api/sync/generations")
      json({
        protocol: 2,
        workspaceId,
        generations: { state: revision, drafts: 1, transcripts: 1 },
      });
    else if (url.pathname === "/api/sync/stream") {
      assert.equal(url.searchParams.get("protocol"), "2");
      assert.equal(url.searchParams.get("scope"), null);
      res.writeHead(200, { "Content-Type": "text/event-stream" });
      res.write(": connected\n\n");
      streams.add(res);
      res.on("close", () => streams.delete(res));
    } else if (url.pathname === "/api/sync/pull") {
      pulls++;
      const row = {
        id: "entity:agent:lead",
        seq: revision,
        _deleted: false,
        payload: JSON.stringify({
          collection: "agent",
          id: "lead",
          value: {
            id: "lead",
            name: revision === 1 ? "Initial lead" : "Pushed lead",
            status: "running",
          },
        }),
      };
      json({
        workspaceId,
        documents:
          Number(url.searchParams.get("after")) >= revision ? [] : [row],
        checkpoint: { seq: revision },
        maxSeq: revision,
        initialHigh: revision,
      });
    } else next();
  },
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
  await page.goto(
    `http://127.0.0.1:${server.httpServer.address().port}/push-check`,
  );
  await page.evaluate(async () => {
    window.client = await import("/src/sync/client.ts");
    window.stopPush = window.client.subscribeProjection(
      "state",
      (value) => {
        window.pushState = value?.threads?.find(
          (row) => row.id === "lead",
        )?.name;
      },
      (error) => {
        if (error) window.pushError = String(error);
      },
    );
  });
  await page.waitForFunction(() => window.pushState === "Initial lead");
  const before = pulls;
  revision = 2;
  for (const res of streams)
    res.write(
      `data: ${JSON.stringify({ protocol: 2, workspaceId, generations: { state: 2, drafts: 1, transcripts: 1 } })}\n\n`,
    );
  await page.waitForFunction(() => window.pushState === "Pushed lead");
  assert.ok(
    pulls > before,
    "The stream does not replace an authoritative pull.",
  );
  assert.equal(streams.size, 1);
  assert.equal(
    await page.evaluate(async () => {
      const { db } = await client.syncDatabase();
      return JSON.parse(
        (await db.projections.findOne("entity:agent:lead").exec()).payload,
      ).value.name;
    }),
    "Pushed lead",
  );
  assert.deepEqual(errors, []);
  assert.equal(await page.evaluate(() => window.pushError), undefined);
  await page.evaluate(() => window.stopPush());
  console.log("sync push browser contract passed");
} finally {
  await browser.close();
  for (const res of streams) res.end();
  await server.close();
}
