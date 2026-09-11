// Real RxDB and EventSource, with an isolated server and simulated PWA lifecycle.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium, webkit } = require("playwright-core");
const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
const { createServer } = await import(
  new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url)
);
const streams = new Set();
let opened = 0;
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
  plugins: [
    {
      name: "sync-stream-fixture",
      configureServer(server) {
        server.middlewares.use("/api/sync/stream", (_request, response) => {
          response.setHeader("Content-Type", "text/event-stream");
          response.setHeader("Cache-Control", "no-cache");
          response.write('data: "RESYNC"\n\n');
          opened++;
          streams.add(response);
          response.on("close", () => streams.delete(response));
        });
      },
    },
  ],
});
await server.listen();
const browser = await engine.launch({
  ...(engine === chromium
    ? {
        executablePath:
          process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      }
    : {}),
  headless: true,
});
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function until(test, message) {
  for (let count = 0; count < 100; count++) {
    if (test()) return;
    await delay(30);
  }
  assert.ok(test(), message);
}
try {
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let revision = 1;
  let identities = 0;
  let chatState = false;
  const pulls = [];
  const pullCursors = [];
  const workspaceId = "e".repeat(32);
  await page.route("**/check", (route) =>
    route.fulfill({ contentType: "text/html", body: "<!doctype html>" }),
  );
  await page.route("**/api/sync/identity", (route) => {
    identities++;
    return route.fulfill({ json: { workspaceId, chatState } });
  });
  await page.route("**/api/sync/pull?*", (route) => {
    const url = new URL(route.request().url());
    const scope = url.searchParams.get("scope");
    const after = Number(url.searchParams.get("after"));
    pulls.push(scope);
    pullCursors.push({ scope, after });
    return route.fulfill({
      json: {
        workspaceId,
        documents:
          scope !== "drafts" && after < revision
            ? [
                {
                  id: scope,
                  seq: revision,
                  payload: JSON.stringify({ revision }),
                },
              ]
            : [],
        checkpoint: { seq: revision },
      },
    });
  });
  await page.route("**/api/sync/drafts", (route) =>
    route.fulfill({ json: [] }),
  );
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.evaluate(async () => {
    const client = await import("/src/sync/client.ts");
    window.values = {};
    window.stops = await Promise.all(
      ["state", "team", "history:lead", "state"].map((scope) =>
        client.watchProjection(
          scope,
          (value) => {
            window.values[scope] = value;
          },
          () => {},
        ),
      ),
    );
    window.stops.push(await client.startDraftReplication(() => {}));
    window.resumeCalls = 0;
    window.stopResume = (await import("/src/sync/resume.ts")).onResume(
      () => window.resumeCalls++,
    );
  });
  await page.waitForFunction(
    () =>
      Object.values(window.values).length === 3 &&
      Object.values(window.values).every((v) => v?.revision === 1),
  );
  await until(() => streams.size === 1, "All scopes share one live stream");
  assert.equal(opened, 1, "Mounting three scopes and drafts opens one stream");
  await delay(250);
  const beforeBurst = pulls.length;
  revision = 2;
  for (let index = 0; index < 30; index++)
    for (const stream of streams) stream.write('data: "RESYNC"\n\n');
  await page.waitForFunction(() =>
    Object.values(window.values).every((v) => v?.revision === 2),
  );
  await delay(150);
  assert.equal(
    pulls.length - beforeBurst,
    4,
    "A burst causes one pull per distinct scope, including drafts",
  );

  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      value: true,
    });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  await until(
    () => streams.size === 0,
    "Backgrounding closes the shared socket",
  );
  await delay(100);
  const hiddenPulls = pulls.length;
  await delay(3200);
  assert.equal(pulls.length, hiddenPulls, "Background fallback polling stops");
  revision = 3;
  const resumedAt = Date.now();
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      value: false,
    });
    document.dispatchEvent(new Event("visibilitychange"));
    window.dispatchEvent(new Event("pageshow"));
    window.dispatchEvent(new Event("online"));
  });
  await page.waitForFunction(() =>
    Object.values(window.values).every((v) => v?.revision === 3),
  );
  assert.ok(
    Date.now() - resumedAt < 1500,
    "Visible state recovers without waiting for fallback polling",
  );
  await page.waitForFunction(() => window.resumeCalls === 1);
  await until(() => opened === 2, "Resume opens the replacement stream");
  assert.equal(
    await page.evaluate(() => window.resumeCalls),
    1,
    "One PWA return invokes each resume consumer once",
  );
  assert.equal(opened, 2, "One PWA return creates one replacement stream");

  await page.evaluate(() => {
    Object.defineProperty(navigator, "onLine", {
      configurable: true,
      value: false,
    });
    window.dispatchEvent(new Event("offline"));
  });
  await until(() => streams.size === 0, "Offline closes the stream");
  await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
  await delay(120);
  assert.equal(
    opened,
    2,
    "History restore does not open a socket while offline",
  );
  await page.evaluate(() => {
    Object.defineProperty(navigator, "onLine", {
      configurable: true,
      value: true,
    });
    window.dispatchEvent(new Event("online"));
  });
  await until(() => streams.size === 1, "Online recreates the stream");
  assert.equal(opened, 3);
  await page.evaluate(() => {
    for (const stop of window.stops) stop();
    window.stopResume();
  });
  await until(() => streams.size === 0, "Last subscription closes the stream");
  await delay(100);
  const stoppedPulls = pulls.length;
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await delay(3200);
  assert.equal(
    pulls.length,
    stoppedPulls,
    "Unmounted subscriptions leave no polling work",
  );

  chatState = true;
  revision = 4;
  const beforeCompact = pulls.length;
  await page.reload();
  await page.evaluate(async () => {
    window.value = null;
    window.stopCompact = await (
      await import("/src/sync/client.ts")
    ).watchProjection(
      "state",
      (value) => (window.value = value),
      () => {},
    );
  });
  await page.waitForFunction(() => window.value?.revision === 4);
  assert.equal(
    pullCursors.find((pull) => pull.scope === "state:chat").after,
    0,
    "The new projection starts with its own checkpoint",
  );
  assert.ok(
    pulls.slice(beforeCompact).includes("state:chat"),
    "Advertised compact state uses its own server scope",
  );
  assert.ok(
    !pulls.slice(beforeCompact).includes("state"),
    "Compact clients do not fetch the full state projection",
  );
  const localState = await page.evaluate(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    return {
      state: JSON.parse((await db.projections.findOne("state").exec()).payload),
      remote: Boolean(await db.projections.findOne("state:chat").exec()),
    };
  });
  assert.equal(
    localState.state.revision,
    4,
    "Capability upgrade replaces the existing cached state",
  );
  assert.equal(
    localState.remote,
    false,
    "The local state identity stays compatible with all existing subscribers",
  );
  await page.evaluate(() => window.stopCompact());

  await page.addInitScript(() => {
    Object.defineProperty(navigator, "onLine", {
      configurable: true,
      value: false,
    });
  });
  const beforeOfflineIdentity = identities;
  await page.reload();
  const cached = await page.evaluate(async () => {
    const started = performance.now();
    const { db, workspaceId } = await (
      await import("/src/sync/client.ts")
    ).syncDatabase();
    return {
      workspaceId,
      value: JSON.parse((await db.projections.findOne("state").exec()).payload),
      elapsed: performance.now() - started,
    };
  });
  assert.equal(cached.workspaceId, workspaceId);
  assert.equal(
    cached.value.revision,
    4,
    "Offline startup retains the cached server projection",
  );
  assert.equal(
    identities,
    beforeOfflineIdentity,
    "Explicit offline startup does not wait for identity HTTP",
  );
  assert.ok(
    cached.elapsed < 1500,
    "Offline database opens without the HTTP deadline",
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS: one shared SSE, coalesced pulls, hidden/offline suspension, resume recovery, complete disposal, cached offline startup",
  );
} finally {
  await browser.close();
  for (const response of streams) response.end();
  await server.close();
}
