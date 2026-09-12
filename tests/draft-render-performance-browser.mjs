// Real draft hook, local durability, and consumer commits with 500 saved chats.
import { createRequire } from "node:module";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../", import.meta.url));
const require = createRequire(root + "/web/package.json");
const { chromium, webkit } = require("playwright-core");
const engine = process.env.BROWSER === "webkit" ? webkit : chromium;
const { createServer } = await import(
  root + "/web/node_modules/vite/dist/node/index.js"
);
const server = await createServer({
  configFile: false,
  root: root + "/web",
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
  for (const count of [500]) {
    const context = await browser.newContext();
    const page = await context.newPage();
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const workspaceId = "d".repeat(32);
    await page.route("**/check", (r) =>
      r.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Isolated draft hook audit</title>",
      }),
    );
    await page.route("**/api/sync/identity", (r) =>
      r.fulfill({ json: { workspaceId } }),
    );
    await page.route("**/api/sync/drafts", (r) => r.fulfill({ json: [] }));
    await page.route("**/api/sync/pull?*", (r) =>
      r.fulfill({
        json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
      }),
    );
    await page.route("**/api/sync/stream", (r) =>
      r.fulfill({ contentType: "text/event-stream", body: "" }),
    );
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(
      async ({ count, workspaceId }) => {
        localStorage.setItem(
          "codex-sync-workspace",
          JSON.stringify(workspaceId),
        );
        const drafts = {};
        for (let i = 0; i < count; i++) drafts["chat-" + i] = "x".repeat(2000);
        localStorage.setItem(
          "codex-drafts:" + workspaceId,
          JSON.stringify(drafts),
        );
        const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
        const { db } = await (
          await import("/src/sync/client.ts")
        ).syncDatabase();
        window.db = db;
        const docs = Object.entries(drafts).map(([session, text]) => ({
          id: "fixture:" + session,
          seq: 0,
          payload: JSON.stringify({
            id: "fixture:" + session,
            device: "fixture",
            session,
            text,
            updated: 1,
          }),
        }));
        await db.drafts.bulkInsert(docs);
        const r = await import("/node_modules/.vite/deps/react.js"),
          d = await import("/node_modules/.vite/deps/react-dom_client.js");
        const React = r.default || r,
          { createRoot } = d.default || d;
        const node = document.createElement("div");
        document.body.append(node);
        window.commits = 0;
        window.renders = 0;
        createRoot(node).render(
          React.createElement(
            React.Profiler,
            { id: "draft", onRender: () => window.commits++ },
            React.createElement(function Harness() {
              window.renders++;
              window.draft = useSyncedDrafts();
              return null;
            }),
          ),
        );
      },
      { count, workspaceId },
    );
    await page.waitForFunction(
      () => window.draft?.drafts["chat-0"] === "x".repeat(2000),
    );
    await page.waitForTimeout(1200);
    if (engine === chromium) {
      const cdp = await context.newCDPSession(page);
      await cdp.send("Emulation.setCPUThrottlingRate", { rate: 4 });
    }
    const result = await page.evaluate(async (workspaceId) => {
      const baseCommits = window.commits,
        baseRenders = window.renders;
      let mapWrites = 0,
        mapBytes = 0,
        journalWrites = 0;
      const original = Storage.prototype.setItem;
      Storage.prototype.setItem = function (k, v) {
        if (k.startsWith("codex-drafts:")) {
          if (k.includes(":pending:")) journalWrites++;
          else {
            mapWrites++;
            mapBytes += v.length * 2;
          }
        }
        return original.call(this, k, v);
      };
      const setTimes = [];
      for (let i = 0; i < 20; i++) {
        const at = performance.now();
        window.draft.setDrafts((old) => ({ ...old, "chat-0": "typed " + i }));
        if (
          JSON.parse(localStorage.getItem("codex-drafts:" + workspaceId))[
            "chat-0"
          ] !==
          "typed " + i
        )
          throw new Error("Each edit must synchronously reach the scope map");
        if (
          !Object.keys(localStorage).some(
            (k) =>
              k.includes(":pending:") &&
              JSON.parse(localStorage.getItem(k)).text === "typed " + i,
          )
        )
          throw new Error("Each edit must synchronously reach the journal");
        setTimes.push(performance.now() - at);
        await new Promise((r) => setTimeout(r, 40));
      }
      await new Promise((r) => setTimeout(r, 500));
      Storage.prototype.setItem = original;
      const sorted = setTimes.slice().sort((a, b) => a - b);
      return {
        setMedianMs: sorted[10],
        setMaxMs: Math.max(...setTimes),
        commits: window.commits - baseCommits,
        renders: window.renders - baseRenders,
        mapWrites,
        mapBytes,
        journalWrites,
      };
    }, workspaceId);
    assert.equal(
      result.mapWrites,
      20,
      "Local sync must not rewrite all drafts for unchanged reconciliation",
    );
    assert.equal(
      result.journalWrites,
      20,
      "Each input keeps its synchronous journal write",
    );
    assert.ok(
      result.commits <= 22,
      `Local persistence must not force extra consumer commits: ${result.commits}`,
    );
    assert.ok(
      result.mapBytes < 45_000_000,
      "Twenty edits do not serialize the map three times per edit",
    );
    const noOp = await page.evaluate(async () => {
      const before = window.commits;
      const original = Storage.prototype.setItem;
      let writes = 0;
      Storage.prototype.setItem = function (k, v) {
        if (k.startsWith("codex-drafts:")) writes++;
        return original.call(this, k, v);
      };
      const doc = await db.drafts.findOne("fixture:chat-10").exec();
      await doc.incrementalPatch({ seq: 100 });
      await new Promise((r) => setTimeout(r, 300));
      Storage.prototype.setItem = original;
      return { commits: window.commits - before, writes };
    });
    assert.deepEqual(
      noOp,
      { commits: 0, writes: 0 },
      "Unchanged remote text preserves consumer state and local storage",
    );
    await page.evaluate(async () => {
      await db.drafts.incrementalUpsert({
        id: "remote:chat-0",
        seq: 200,
        payload: JSON.stringify({
          id: "remote:chat-0",
          session: "chat-0",
          device: "remote",
          updated: Date.now() + 1000,
          text: "Concurrent remote text",
        }),
      });
    });
    await page.waitForFunction(
      () =>
        draft.drafts["chat-0"] === "Concurrent remote text" &&
        draft.conflicts.some((v) => v.text === "typed 19"),
    );
    await page.evaluate(() =>
      draft.dismissDraft(draft.conflicts.find((v) => v.text === "typed 19")),
    );
    await page.waitForFunction(
      () =>
        draft.drafts["chat-0"] === "Concurrent remote text" &&
        !draft.conflicts.some((v) => v.text === "typed 19"),
    );
    assert.equal(
      await page.evaluate(() => draft.drafts["chat-499"]),
      "x".repeat(2000),
      "Unrelated drafts remain exact",
    );
    const other = await context.newPage();
    other.on("pageerror", (error) => errors.push(error.message));
    await other.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Second draft tab</title>",
      }),
    );
    await other.route("**/api/sync/identity", (route) =>
      route.fulfill({ json: { workspaceId } }),
    );
    await other.route("**/api/sync/drafts", (route) =>
      route.fulfill({ json: [] }),
    );
    await other.route("**/api/sync/pull?*", (route) =>
      route.fulfill({
        json: { workspaceId, documents: [], checkpoint: { seq: 0 } },
      }),
    );
    await other.route("**/api/sync/stream", (route) =>
      route.fulfill({ contentType: "text/event-stream", body: "" }),
    );
    await other.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await other.evaluate(async () => {
      const { useSyncedDrafts } = await import("/src/sync/drafts.ts");
      const r = await import("/node_modules/.vite/deps/react.js");
      const d = await import("/node_modules/.vite/deps/react-dom_client.js");
      const React = r.default || r;
      const { createRoot } = d.default || d;
      const node = document.createElement("div");
      document.body.appendChild(node);
      createRoot(node).render(
        React.createElement(function Harness() {
          window.draft = useSyncedDrafts();
          return null;
        }),
      );
    });
    await other.waitForFunction(
      () => window.draft?.drafts["chat-0"] === "Concurrent remote text",
    );
    await other.evaluate(() =>
      draft.setDrafts((old) => ({ ...old, "chat-1": "Exact second tab edit" })),
    );
    await page.waitForFunction(
      () => draft.drafts["chat-1"] === "Exact second tab edit",
    );
    assert.equal(
      await page.evaluate(() => draft.drafts["chat-0"]),
      "Concurrent remote text",
      "A second tab edit does not replace another chat",
    );
    assert.equal(
      await other.evaluate(() =>
        draft.conflicts.some((version) => version.text === "typed 19"),
      ),
      false,
      "Conflict dismissal survives a new tab",
    );
    await other.close();
    assert.deepEqual(errors, [], "Both draft consumers have no runtime errors");
    console.log(
      JSON.stringify({
        browser: process.env.BROWSER || "chromium",
        count,
        edits: 20,
        ...result,
        noOp,
      }),
    );
    await context.close();
  }
} finally {
  await browser.close();
  await server.close();
}
