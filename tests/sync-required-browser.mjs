// Unsupported endpoints fail visibly and never bypass the durable outbox.
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
await server.listen();
const browser = await chromium.launch({
  executablePath:
    process.env.CHROME_BIN ||
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  headless: true,
});
try {
  const page = await browser.newPage(),
    errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  let status = 404,
    probes = 0,
    sends = 0;
  await page.route("**/sync-check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Required sync</title>",
    }),
  );
  await page.route("**/api/sync/identity", (route) => {
    probes++;
    return route.fulfill({
      status,
      json:
        status === 200
          ? { workspaceId: "a".repeat(32) }
          : { error: "Identity unavailable" },
    });
  });
  await page.route("**/api/messages", (route) => {
    sends++;
    return route.fulfill({ json: { status: "queued" } });
  });
  const url = `http://127.0.0.1:${server.httpServer.address().port}/sync-check`;
  const mount = async () =>
    page.evaluate(async () => {
      const { useOutbox } = await import("/src/sync/send.ts");
      const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
      const r = await import("/node_modules/.vite/deps/react.js"),
        d = await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = r.default || r,
        { createRoot } = d.default || d;
      const root = document.createElement("div");
      document.body.appendChild(root);
      createRoot(root).render(
        React.createElement(function Harness() {
          const outbox = useOutbox(),
            drafts = useSyncedDrafts();
          window.state = { outbox, drafts };
          return null;
        }),
      );
    });
  await page.goto(url);
  await mount();
  await page.waitForFunction(() => window.state);
  await page.evaluate(() =>
    window.state.drafts.setDrafts({ lead: "Saved local draft" }),
  );
  await page.waitForTimeout(250);
  assert.ok(probes > 0);
  assert.equal(
    await page.evaluate(
      () => JSON.parse(localStorage.getItem("codex-drafts:unassigned"))?.lead,
    ),
    "Saved local draft",
  );
  assert.equal(
    await page.evaluate(() => localStorage.getItem("codex-drafts:legacy")),
    null,
  );
  await page.waitForFunction(
    () => window.state.outbox.error && window.state.drafts.error,
  );
  const sendError = await page.evaluate(async () => {
    try {
      await (
        await import("/src/sync/send.ts")
      ).durableSend({ id: "missing-sync", room: "lead", text: "message" });
    } catch (error) {
      return error.message;
    }
  });
  assert.match(sendError, /Identity unavailable/);
  assert.equal(sends, 0);
  await page.reload();
  await mount();
  await page.waitForFunction(
    () => window.state?.drafts.drafts.lead === "Saved local draft",
  );
  status = 503;
  await page.reload();
  await mount();
  await page.waitForFunction(
    () => window.state?.drafts.error && window.state?.outbox.error,
  );
  assert.equal(
    await page.evaluate(() => window.state.drafts.error),
    "Draft sync paused. Retrying automatically.",
  );
  // Initial connection failures retry without a reload or loss of the local draft.
  await page.route("**/api/sync/pull?*", (route) =>
    route.fulfill({
      json: {
        workspaceId: "a".repeat(32),
        documents: [],
        checkpoint: { seq: 0 },
      },
    }),
  );
  await page.route("**/api/sync/drafts", (route) =>
    route.fulfill({ json: [] }),
  );
  status = 200;
  await page.waitForFunction(() => window.state.drafts.error === "");
  assert.equal(
    await page.evaluate(() => window.state.drafts.drafts.lead),
    "Saved local draft",
  );
  // Successful remote reads cannot clear a local storage failure.
  await page.evaluate(async () => {
    const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
    const original = db.drafts.incrementalUpsert;
    window.restoreDraftWrites = () => {
      db.drafts.incrementalUpsert = original;
    };
    db.drafts.incrementalUpsert = async () => {
      throw new Error("Local write fixture");
    };
    window.state.drafts.setDrafts({ lead: "Unsaved local edit" });
  });
  await page.waitForFunction(() =>
    window.state.drafts.error.includes("could not be saved"),
  );
  await page.waitForTimeout(3500);
  assert.match(
    await page.evaluate(() => window.state.drafts.error),
    /could not be saved/,
  );
  await page.evaluate(() => {
    window.restoreDraftWrites();
    window.state.drafts.setDrafts({ lead: "Recovered local edit" });
  });
  await page.waitForFunction(() => window.state.drafts.error === "");
  assert.deepEqual(errors, []);
  console.log("sync endpoint requirement browser contract passed");
} finally {
  await browser.close();
  await server.close();
}
