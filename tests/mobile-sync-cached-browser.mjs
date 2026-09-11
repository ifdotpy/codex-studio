// Cached UI appears while an online Mac is unreachable; identity gates replication.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium, webkit } = require("playwright-core");
const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
const { createServer } = await import(
  new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url)
);
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
});
await server.listen();
const browser = await engine.launch({
  headless: true,
  ...(engine === chromium
    ? {
        executablePath:
          process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      }
    : {}),
});
try {
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const workspaceId = "f".repeat(32);
  let identityCalls = 0;
  let currentWorkspace = workspaceId;
  let holdIdentity = false,
    identityRoute,
    pullCount = 0,
    pushCount = 0;
  await page.route("**/check", (route) =>
    route.fulfill({ contentType: "text/html", body: "<!doctype html>" }),
  );
  await page.route("**/api/sync/identity", (route) => {
    identityCalls++;
    if (holdIdentity) {
      identityRoute = route;
      return;
    }
    return route.fulfill({
      json: { workspaceId: currentWorkspace, chatState: true },
    });
  });
  await page.route("**/api/sync/stream", (route) =>
    route.fulfill({ contentType: "text/event-stream", body: "" }),
  );
  await page.route("**/api/sync/pull?*", (route) => {
    pullCount++;
    const url = new URL(route.request().url());
    const scope = url.searchParams.get("scope");
    const after = Number(url.searchParams.get("after"));
    return route.fulfill({
      json: {
        workspaceId,
        documents:
          scope === "state:chat" && after < 2
            ? [
                {
                  id: scope,
                  seq: 2,
                  payload: JSON.stringify({ marker: "fresh" }),
                },
              ]
            : [],
        checkpoint: { seq: 2 },
      },
    });
  });
  await page.route("**/api/sync/drafts", (route) => {
    pushCount++;
    return route.fulfill({ json: [] });
  });
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.evaluate(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    await db.projections.insert({
      id: "state",
      seq: 1,
      payload: JSON.stringify({ marker: "cached" }),
    });
    await db.drafts.insert({
      id: "phone:lead",
      seq: 0,
      payload: JSON.stringify({
        id: "phone:lead",
        text: "Never send to another workspace",
        session: "lead",
        device: "phone",
        updated: 1,
      }),
    });
  });
  holdIdentity = true;
  await page.reload();
  const started = Date.now();
  await page.evaluate(async () => {
    const client = await import("/src/sync/client.ts");
    window.syncErrors = [];
    window.stopState = await client.watchProjection(
      "state",
      (value) => (window.state = value),
      (error) => {
        if (error) window.syncErrors.push(String(error));
      },
    );
    window.stopDrafts = await client.startDraftReplication((error) => {
      if (error) window.syncErrors.push(String(error));
    });
  });
  await page.waitForFunction(() => window.state?.marker === "cached");
  const cachedMs = Date.now() - started;
  assert.ok(cachedMs < 1500, `Cached state waits for the Mac: ${cachedMs} ms`);
  assert.ok(
    await page.evaluate(() => navigator.onLine),
    "The test reproduces an online phone with an unreachable Mac",
  );
  assert.equal(
    pullCount,
    0,
    "Unverified cached identity permits no projection reads",
  );
  assert.equal(
    pushCount,
    0,
    "Unverified cached identity permits no draft writes",
  );
  assert.ok(identityRoute);
  holdIdentity = false;
  currentWorkspace = "a".repeat(32);
  await identityRoute.fulfill({
    json: { workspaceId: "a".repeat(32), chatState: true },
  });
  await page.waitForFunction(() =>
    window.syncErrors.some((error) => error.includes("workspace changed")),
  );
  assert.equal(
    pullCount,
    0,
    "A different server cannot enter the cached database",
  );
  assert.equal(pushCount, 0, "A different server cannot receive cached drafts");
  assert.equal(await page.evaluate(() => window.state.marker), "cached");

  // A failed check is not cached forever. Reconnecting to the saved workspace recovers.
  currentWorkspace = workspaceId;
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await page
    .waitForFunction(() => window.state?.marker === "fresh", null, {
      timeout: 10000,
    })
    .catch(async (error) => {
      console.error({
        identityCalls,
        pullCount,
        pushCount,
        state: await page.evaluate(() => ({
          value: window.state,
          errors: window.syncErrors,
          hidden: document.hidden,
          online: navigator.onLine,
        })),
      });
      throw error;
    });
  for (let attempt = 0; attempt < 100 && !pushCount; attempt++)
    await new Promise((resolve) => setTimeout(resolve, 30));
  assert.ok(
    pushCount > 0,
    "Draft replication resumes after exact workspace verification",
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS: cached online startup ${cachedMs} ms, blocked cross-workspace reads/writes, verified retry recovery`,
  );
} finally {
  await browser.close();
  await server.close();
}
