// Real queue callers, local HTTP receipts, and no native model requests.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(root, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const temporary = await mkdtemp(join(tmpdir(), "studio-queue-deadline-"));
const workspaceId = "c".repeat(32);
const server = await createServer({
  configFile: false,
  root: join(root, "web"),
  cacheDir: join(temporary, "vite"),
  optimizeDeps: { include: ["react", "react-dom/client"] },
  plugins: [
    {
      name: "queue-deadline-harness",
      resolveId(id) {
        if (id === "virtual:queue-deadline") return "\0" + id;
      },
      load(id) {
        if (id === "\0virtual:queue-deadline")
          return `
            import React from "react";
            import { createRoot } from "react-dom/client";
            import { useMessageQueue } from "/src/components/useMessageQueue.ts";
            export function mount() {
              const root = createRoot(document.body);
              root.render(React.createElement(function Harness() {
                window.queue = useMessageQueue({
                  id: "chat", enabled: true, scope: "deadline-fixture",
                  workspaceId: "${workspaceId}", refresh: async () => {},
                  observed: ids => window.observed.push(ids),
                  edited: (id, text) => window.edited.push({id, text}),
                });
                return null;
              }));
              return root;
            }
          `;
      },
    },
  ],
  server: { host: "127.0.0.1", port: 0, hmr: false },
});
await server.listen();
let browser;
try {
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage();
  page.setDefaultTimeout(5000);
  const errors = [],
    posts = [],
    nativeRequests = [],
    receipts = new Map();
  let view = { items: [], revision: "initial" };
  let applied = 0;
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    if (
      ["/api/messages", "/api/stop"].includes(new URL(request.url()).pathname)
    )
      nativeRequests.push(request.url());
  });
  await page.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Queue deadlines</title>",
    }),
  );
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ json: { workspaceId } }),
  );
  await page.route(/\/api\/queue(?:\?.*)?$/, (route) => {
    if (route.request().method() !== "POST")
      return route.fulfill({
        json: {
          ...view,
          capabilities: { receipts: true, reorder: true },
        },
      });
    const body = route.request().postDataJSON();
    posts.push(body);
    const previous = receipts.get(body.request_id);
    if (previous) {
      assert.deepEqual(
        body,
        previous.body,
        "A retry must preserve every field",
      );
      return route.fulfill({ json: previous.reply });
    }
    assert.equal(body.expected_revision, view.revision);
    assert.equal(route.request().headers()["x-canvas-workspace"], workspaceId);
    const reply = { ok: true };
    receipts.set(body.request_id, { body, reply });
    applied++;
    if (body.action === "cancel")
      view.items = view.items.filter((item) => item.id !== body.id);
    else if (body.action === "edit")
      view.items = view.items.map((item) =>
        item.id === body.id ? { ...item, text: body.text } : item,
      );
    else if (body.action === "reorder")
      view.items = body.ordered_ids.map((id) =>
        view.items.find((item) => item.id === id),
      );
    else assert.fail(`Unexpected queue action: ${body.action}`);
    view.revision += ":committed";
    // The operation commits, but its response stays pending until the deadline.
  });
  await page.goto(server.resolvedUrls.local[0] + "check");
  await page.evaluate(async () => {
    window.observed = [];
    window.edited = [];
    window.advance = 0;
    const now = Date.now;
    Date.now = () => now() + window.advance;
    window.reactRoot = (
      await import("/@id/__x00__virtual:queue-deadline")
    ).mount();
  });
  await page.waitForFunction(() => window.queue && !window.queue.loading);
  const untilPosts = async (count) => {
    const deadline = Date.now() + 5000;
    while (posts.length < count) {
      assert.ok(Date.now() < deadline, "The queue POST must start");
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  };
  const expire = () =>
    page.evaluate(() => {
      window.advance += 16000;
      window.dispatchEvent(new Event("pageshow"));
    });
  for (const action of ["cancel", "edit", "reorder"]) {
    view = {
      items: [1, 2].map((number) => ({
        id: `${action}-${number}`,
        kind: "user",
        text: `Exact ${action} ${number}`,
      })),
      revision: `revision-${action}`,
    };
    await page.evaluate(() => window.queue.reload());
    await page.waitForFunction(
      (action) => window.queue.items[0]?.id === `${action}-1`,
      action,
    );
    const before = posts.length;
    await page.evaluate((action) => {
      window.outcome = "pending";
      const queue = window.queue;
      const operation =
        action === "edit"
          ? queue.edit(queue.items[0], "Exact replacement")
          : action === "reorder"
            ? queue.reorder(queue.items.map((item) => item.id).reverse())
            : queue.cancel(queue.items[0]);
      window.operation = operation.then(
        () => (window.outcome = "success"),
        (error) => (window.outcome = error.name),
      );
    }, action);
    await untilPosts(before + 1);
    const request = posts[before];
    const observed = await page.evaluate(() => ({
      observed: window.observed.length,
      edited: window.edited.length,
    }));
    await expire();
    await page.waitForFunction(
      () => window.outcome !== "pending" && !window.queue.busy,
    );
    assert.equal(
      await page.evaluate(() => window.outcome),
      "NetworkTimeoutError",
    );
    assert.deepEqual(await page.evaluate(() => window.queue.pending), request);
    assert.deepEqual(
      await page.evaluate(() => ({
        observed: window.observed.length,
        edited: window.edited.length,
      })),
      observed,
      "An unknown result cannot report confirmed cancellation or editing",
    );
    assert.match(
      await page.evaluate(() =>
        window.queue
          .cancel(window.queue.items[0])
          .catch((error) => error.message),
      ),
      /Retry the previous queue change/,
    );
    assert.equal(posts.length, before + 1);
    await page.evaluate(() => window.queue.retry());
    assert.deepEqual(posts[before + 1], request);
    assert.equal(await page.evaluate(() => window.queue.pending), null);
    assert.equal(await page.evaluate(() => window.queue.busy), false);
  }
  await page.evaluate(() => window.reactRoot.unmount());
  assert.equal(applied, 3);

  // Removal has its own saved request. Its retry must work after the row is gone.
  view = {
    items: [{ id: "remove-1", kind: "user", text: "Exact removal" }],
    revision: "revision-remove",
  };
  const before = posts.length;
  await page.evaluate(async (workspaceId) => {
    window.removal = await import("/src/components/removeSendingMessages.ts");
    window.removed = await import("/src/components/removedMessages.ts");
    window.scope = { stateDir: "deadline-fixture", workspaceId };
    window.target = {
      chat: "chat",
      message: {
        id: "chat:remove-1",
        clientMessageId: "remove-1",
        role: "user",
        text: "Exact removal",
        deliveryStatus: "sending",
      },
    };
    window.outcome = "pending";
    window.operation = window.removal
      .removeSendingMessage(window.scope, window.target)
      .then(
        () => (window.outcome = "success"),
        (error) => (window.outcome = error.name),
      );
  }, workspaceId);
  await untilPosts(before + 1);
  const request = posts[before];
  await expire();
  await page.waitForFunction(() => window.outcome !== "pending");
  assert.equal(
    await page.evaluate(() => window.outcome),
    "NetworkTimeoutError",
  );
  assert.equal(
    await page.evaluate(() =>
      window.removed.isRemovedMessage(
        window.scope.stateDir,
        "agent",
        "chat",
        window.target.message,
      ),
    ),
    false,
    "An unknown queue result must remain available for exact retry",
  );
  await page.evaluate(() =>
    window.removal.removeSendingMessage(window.scope, window.target),
  );
  assert.deepEqual(posts[before + 1], request);
  assert.equal(applied, 4);
  assert.equal(
    await page.evaluate(() =>
      window.removed.isRemovedMessage(
        window.scope.stateDir,
        "agent",
        "chat",
        window.target.message,
      ),
    ),
    true,
  );
  assert.deepEqual(nativeRequests, []);
  assert.deepEqual(errors, []);
  console.log(
    "PASS: queue cancel/edit/reorder and removal expire, preserve exact requests, and retry one committed effect",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(temporary, { recursive: true, force: true });
}
