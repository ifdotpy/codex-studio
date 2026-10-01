import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { join } from "node:path";

const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const server = await createServer({
  configFile: false,
  root,
  optimizeDeps: { noDiscovery: true },
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "renderer-performance-measure",
      configureServer(vite) {
        vite.middlewares.use("/renderer-perf-measure", (_request, response) => {
          response.setHeader("Content-Type", "text/html");
          response.end(`<!doctype html><div id="root"></div><script type="module">
            import { applyEntityRows, emptyEntityProjection } from "/src/sync/entityProjection.ts";
            import { boundTranscriptItems } from "/src/transcriptPageBounds.ts";
            window.measureRenderer = () => {
              const rows = Array.from({ length: 2090 }, (_, index) => {
                const collection = index % 3 === 0 ? "agent" : index % 3 === 1 ? "task" : "event";
                const id = String(index);
                const value = { id, name: "Entity " + index, detail: "x".repeat(32) };
                return { id: "entity:" + collection + ":" + id, seq: index + 1,
                  payload: JSON.stringify({ collection, id, value }) };
              });
              const emissionAt = (seq) => {
                const emission = rows.slice();
                emission[1044] = { ...rows[1044], seq,
                  payload: JSON.stringify({ collection: "agent", id: "1044", value: { id: "1044", name: "Changed" } }) };
                return emission;
              };
              const buildFull = (emission) => {
                const full = new Map();
                for (const row of emission) {
                  const entity = JSON.parse(row.payload);
                  let collection = full.get(entity.collection);
                  if (!collection) full.set(entity.collection, collection = new Map());
                  collection.set(entity.id, entity.value);
                }
                const list = (name) => [...(full.get(name)?.values() || [])];
                const agents = list("agent");
                const chats = list("chat");
                return { threads: agents, chats, nodes: [...agents, ...chats],
                  runtime: { agents, tasks: list("task"), events: list("event") } };
              };
              let parses = 0;
              const originalParse = JSON.parse;
              JSON.parse = (...args) => { parses++; return originalParse(...args); };
              const firstEmission = emissionAt(4000);
              const beforeSamples = [];
              for (let index = 0; index < 12; index++) {
                const start = performance.now();
                buildFull(emissionAt(7000 + index));
                if (index >= 2) beforeSamples.push(performance.now() - start);
              }
              const beforeMs = beforeSamples.sort((a, b) => a - b)[Math.floor(beforeSamples.length / 2)];
              parses = 0;
              buildFull(firstEmission);
              const beforeParses = parses;
              const state = emptyEntityProjection();
              const initialSnapshot = applyEntityRows(state, rows, true);
              const deltaAt = (seq) => applyEntityRows(state, emissionAt(seq), true);
              const afterSamples = [];
              for (let index = 0; index < 12; index++) {
                const start = performance.now();
                deltaAt(5000 + index);
                if (index >= 2) afterSamples.push(performance.now() - start);
              }
              parses = 0;
              const snapshot = deltaAt(6000);
              const afterParses = parses;
              const afterMs = afterSamples.sort((a, b) => a - b)[Math.floor(afterSamples.length / 2)];
              JSON.parse = originalParse;

              const transcript = Array.from({ length: 5000 }, (_, index) => ({
                id: "message-" + index, role: "assistant", text: "x".repeat(120),
              }));
              const beforeItems = transcript.length;
              const retained = boundTranscriptItems(
                transcript,
                (item) => JSON.stringify(item).length * 2,
                "newest",
              );
              return {
                entityRows: rows.length,
                before: { parses: beforeParses, ms: beforeMs, emissionMs: beforeMs },
                after: { parses: afterParses, ms: afterMs,
                  unchangedAgentStable: snapshot.threads.find((item) => item.id === "0") ===
                    initialSnapshot.threads.find((item) => item.id === "0"),
                  unchangedCollectionStable: snapshot.runtime.events === initialSnapshot.runtime.events },
                transcript: { beforeItems, afterItems: retained.items.length,
                  afterBytes: retained.bytes },
              };
            };
            window.measureReady = true;
          </script>`);
        });
      },
    },
  ],
});

let browser;
try {
  await server.listen();
  const address = server.httpServer.address();
  const origin = `http://127.0.0.1:${address.port}`;
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1280, height: 800 },
  });
  let blockedWrites = 0;
  await page.route("**/*", async (route) => {
    if (!["GET", "HEAD"].includes(route.request().method())) {
      blockedWrites++;
      return route.abort();
    }
    return route.continue();
  });
  let liveUi = "unavailable";
  try {
    const response = await page.goto("http://127.0.0.1:4620", {
      waitUntil: "domcontentloaded",
      timeout: 10000,
    });
    liveUi = `${response?.status() || "no response"}`;
    await page.locator("body").waitFor({ state: "visible", timeout: 3000 });
  } catch {
    // The isolated measurement remains useful when the live UI is offline.
  }
  await page.goto(`${origin}/renderer-perf-measure`, { waitUntil: "load" });
  await page.waitForFunction(() => window.measureReady === true);
  const result = await page.evaluate(() => window.measureRenderer());
  assert.equal(result.entityRows, 2090);
  assert.equal(result.before.parses, 2090);
  assert.equal(result.after.parses, 1);
  assert.equal(result.after.unchangedAgentStable, true);
  assert.equal(result.after.unchangedCollectionStable, true);
  assert.equal(result.transcript.beforeItems, 5000);
  assert.equal(result.transcript.afterItems, 240);
  console.log(
    JSON.stringify({ headless: true, liveUi, blockedWrites, ...result }),
  );
} finally {
  await browser?.close();
  server.httpServer.closeAllConnections();
  await server.close();
}
