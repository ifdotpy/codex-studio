// Real IndexedDB. Fail local writes, reload, and recover without losing text.
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
  let workspaceId = "b".repeat(32);
  const pushes = [];
  await page.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Draft recovery</title>",
    }),
  );
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ json: { workspaceId } }),
  );
  await page.route("**/api/sync/drafts", (route) => {
    pushes.push(...route.request().postDataJSON().rows);
    return route.fulfill({ json: [] });
  });
  await page.route("**/api/sync/pull?*", (route) =>
    route.fulfill({
      json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
    }),
  );
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  const mount = async (fail) => {
    await page.goto(origin + "/check");
    await page.evaluate(async (fail) => {
      const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
      const { db } = await (await import("/src/sync/client.ts")).syncDatabase();
      window.db = db;
      window.failWrites = fail;
      const write = db.drafts.incrementalUpsert.bind(db.drafts);
      db.drafts.incrementalUpsert = (...args) =>
        window.failWrites
          ? Promise.reject(new Error("Write unavailable"))
          : write(...args);
      const r = await import("/node_modules/.vite/deps/react.js"),
        d = await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = r.default || r,
        { createRoot } = d.default || d;
      const node = document.createElement("div");
      document.body.appendChild(node);
      createRoot(node).render(
        React.createElement(function Harness() {
          window.draft = useSyncedDrafts();
          return null;
        }),
      );
    }, fail);
    await page.waitForFunction(() => !!window.draft);
  };
  const edit = (text) =>
    page.evaluate(
      (text) => window.draft.setDrafts((old) => ({ ...old, lead: text })),
      text,
    );
  const stored = (text) =>
    page.waitForFunction(
      async (text) =>
        (await window.db.drafts.find().exec()).some(
          (doc) => JSON.parse(doc.payload).text === text,
        ),
      text,
    );
  await mount(false);
  await edit("Saved version");
  await stored("Saved version");
  await page.evaluate(() => {
    window.failWrites = true;
  });
  await edit("New draft during a write failure");
  await page.waitForFunction(() => !!window.draft.error);
  await mount(true);
  await page.waitForTimeout(500);
  assert.equal(
    await page.evaluate(() => window.draft.drafts.lead),
    "New draft during a write failure",
    "Reload must not restore the older IndexedDB value over the unsaved edit",
  );
  await page.evaluate(() => {
    window.failWrites = false;
    window.dispatchEvent(new Event("pageshow"));
  });
  await stored("New draft during a write failure");
  await page.waitForFunction(() => !window.draft.error);
  await page.evaluate(() => {
    window.failWrites = true;
  });
  await edit("");
  await page.waitForFunction(() => !!window.draft.error);
  await mount(true);
  await page.waitForTimeout(500);
  assert.equal(
    await page.evaluate(() => window.draft.drafts.lead),
    "",
    "An unsaved clear must remain empty after reload",
  );
  await page.evaluate(() => {
    window.failWrites = false;
    window.dispatchEvent(new Event("pageshow"));
  });
  await stored("");
  await page.waitForFunction(() => !window.draft.error);
  await page.evaluate(() => {
    for (let i = 0; i < 20; i++)
      window.draft.setDrafts((old) => ({ ...old, lead: `Rapid edit ${i}` }));
  });
  await stored("Rapid edit 19");
  await page.waitForFunction(() => !window.draft.error);
  assert.equal(
    await page.evaluate(() => window.draft.conflicts.length),
    0,
    "Typing must not create conflicts with earlier keystrokes",
  );
  await page.evaluate(() => {
    window.failWrites = true;
  });
  await edit("Workspace A draft");
  await page.waitForFunction(() => !!window.draft.error);
  workspaceId = "e".repeat(32);
  await mount(true);
  await page.waitForTimeout(500);
  assert.equal(
    await page.evaluate(() => window.draft.drafts.lead),
    undefined,
    "Pending drafts must not move to a different workspace",
  );
  assert.ok(
    await page.evaluate(() =>
      Object.keys(localStorage).some((key) =>
        key.startsWith(`codex-drafts:${"b".repeat(32)}:pending:`),
      ),
    ),
  );
  workspaceId = "b".repeat(32);
  await mount(false);
  await stored("Workspace A draft");
  await page.waitForFunction(
    () => window.draft.drafts.lead === "Workspace A draft",
  );
  // A recovered old write preserves, rather than overwrites, a newer tab's text.
  await page.evaluate(() => {
    window.failWrites = true;
  });
  await edit("Recovered concurrent draft");
  await page.waitForFunction(() => !!window.draft.error);
  await page.evaluate(async () => {
    window.failWrites = false;
    const doc = (await window.db.drafts.find().exec())[0];
    const pendingKey = Object.keys(localStorage).find((key) =>
      key.includes(":pending:"),
    );
    const pending = JSON.parse(localStorage.getItem(pendingKey));
    const payload = {
      ...JSON.parse(doc.payload),
      text: "Newer tab draft",
      updated: pending.updated + 10,
      seen: {},
      alternatives: [],
    };
    await window.db.drafts.incrementalUpsert({
      id: doc.id,
      seq: 0,
      payload: JSON.stringify(payload),
    });
    window.failWrites = true;
  });
  await mount(false);
  await page.waitForFunction(
    () =>
      window.draft.drafts.lead === "Newer tab draft" &&
      window.draft.conflicts.some(
        (version) => version.text === "Recovered concurrent draft",
      ),
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS: local write failure, reload, exact draft recovery, clear recovery, rapid edits, workspace isolation, concurrent text, automatic retry",
  );
} finally {
  await browser.close();
  await server.close();
}
