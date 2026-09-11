// Real RxDB, with controlled HTTP failures. No live server writes.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(root, "web/package.json"));
const { chromium, webkit } = require("playwright-core");
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const { createServer } = await import(require.resolve("vite"));
const temporary = await mkdtemp(join(tmpdir(), "studio-mobile-send-"));
const server = await createServer({
  configFile: false,
  plugins: [
    {
      name: "mobile-send-harness",
      resolveId(id) {
        if (id === "virtual:mobile-send-harness") return "\0" + id;
      },
      load(id) {
        if (id === "\0virtual:mobile-send-harness")
          return `
        import React from "react";
        import { createRoot } from "react-dom/client";
        import { useOutbox } from "/src/sync/send.ts";
        export function mount() {
          const root = createRoot(document.body);
          root.render(React.createElement(function Harness() {
            window.outbox = useOutbox();
            return null;
          }));
          return root;
        }
      `;
      },
    },
  ],
  root: join(root, "web"),
  cacheDir: join(temporary, "vite"),
  optimizeDeps: { include: ["react", "react-dom/client"] },
  server: { host: "127.0.0.1", port: 0, hmr: false },
});
await server.listen();
const origin = server.resolvedUrls.local[0];
let browser;
try {
  browser = await browserType.launch({
    headless: true,
    executablePath:
      browserType === chromium
        ? process.env.CHROME_BIN ||
          "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
        : undefined,
  });
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    isMobile: true,
  });
  const errors = [],
    posts = [];
  const pending = new Map();
  let holdSession = false;
  const sessions = [];
  await context.route("**/check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Mobile sends</title>",
    }),
  );
  await context.route("**/api/sync/identity", (route) =>
    route.fulfill({
      json: { workspaceId: "b".repeat(32) },
    }),
  );
  await context.route("**/api/session", (route) => {
    if (holdSession) sessions.push(route);
    else return route.fulfill({ json: { token: "fixture" } });
  });
  await context.route("**/api/messages", (route) => {
    const body = route.request().postDataJSON();
    posts.push(body);
    if (body.id.startsWith("hold-")) {
      const routes = pending.get(body.id) || [];
      routes.push(route);
      pending.set(body.id, routes);
    } else return route.fulfill({ json: { id: body.id, status: "delivered" } });
  });
  const mount = async (page) => {
    page.on("pageerror", (error) => errors.push(error.message));
    page.setDefaultTimeout(10000);
    await page.goto(origin + "check");
    await page.evaluate(async () => {
      window.send = await import("/src/sync/send.ts");
      window.storage = await (
        await import("/src/sync/client.ts")
      ).syncDatabase();
      window.fetchCount = 0;
      const original = window.fetch;
      window.fetch = (...args) => {
        window.fetchCount++;
        return original(...args);
      };
      window.read = async (id) => {
        const doc = await window.storage.db.outbox.findOne(id).exec();
        return doc && JSON.parse(doc.getLatest().payload);
      };
      window.start = (body) => {
        window.persisted = false;
        window.settled = false;
        window.result = window.send
          .durableSend(body, [], () => {
            window.persisted = true;
            window.persistedRecord = window.read(body.id);
          })
          .then((result) => {
            window.settled = true;
            return result;
          });
      };
    });
  };
  const first = await context.newPage(),
    second = await context.newPage();
  await mount(first);
  await mount(second);
  const body = (id) => ({
    id,
    room: "mobile-chat",
    text: `Exact content ${id}`,
    assets: [],
    delivery: "queue",
  });
  const until = async (check) => {
    const deadline = Date.now() + 10000;
    while (!(await check())) {
      if (Date.now() > deadline)
        throw new Error("Expected send state did not arrive");
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
  };
  const stored = (page, id) => page.evaluate((id) => window.read(id), id);
  const start = (page, value) =>
    page.evaluate((value) => window.start(value), value);
  const settle = (page) => page.evaluate(() => window.result);
  const reply = (route, id, status, code = 200) =>
    route.fulfill({
      status: code,
      json: code === 200 ? { id, status } : { error: "Old request failed" },
    });

  // Offline writes do not wait for a network timeout or mark an HTTP attempt.
  await context.setOffline(true);
  const offlineStart = Date.now();
  await start(first, body("offline-first"));
  assert.equal((await settle(first)).queued, true);
  assert.ok(Date.now() - offlineStart < 1000);
  assert.equal(await first.evaluate(() => window.fetchCount), 0);
  assert.deepEqual(await first.evaluate(() => window.persistedRecord), {
    body: body("offline-first"),
    status: "queued",
    attempted: false,
    created: (await stored(first, "offline-first")).created,
    displayPending: true,
    attachments: [],
  });
  assert.equal(posts.length, 0);
  await context.setOffline(false);
  await first.evaluate(async () => {
    window.reactRoot = (
      await import("/@id/__x00__virtual:mobile-send-harness")
    ).mount();
  });
  try {
    await until(
      async () => (await stored(first, "offline-first"))?.status === "accepted",
    );
  } catch (error) {
    console.error({
      errors,
      posts,
      page: await first.evaluate(() => ({
        online: navigator.onLine,
        outbox: window.outbox,
        fetchCount: window.fetchCount,
      })),
    });
    throw error;
  }
  assert.deepEqual(posts, [body("offline-first")]);
  await first.evaluate(() => window.reactRoot.unmount());

  // Persistence releases the caller before HTTP. Another tab can still cancel
  // an unattempted intention while the session read is in progress.
  holdSession = true;
  await start(first, body("cancel-before-post"));
  await until(() => sessions.length === 1);
  assert.equal(await first.evaluate(() => window.persisted), true);
  assert.equal(await first.evaluate(() => window.settled), false);
  assert.equal(
    (await first.evaluate(() => window.persistedRecord)).attempted,
    false,
  );
  await second.evaluate(() =>
    window.send.changeOutbox("cancel-before-post", "cancel"),
  );
  holdSession = false;
  await sessions.shift().fulfill({ json: { token: "fixture" } });
  assert.equal((await settle(first)).status, "cancelled");
  assert.equal(posts.length, 1);

  // A suspended sender must not hold a browser lock over the network. A second
  // tab retries the same ID while the first response remains stalled.
  await start(first, body("hold-locked"));
  await until(() => pending.get("hold-locked")?.length === 1);
  const retryStart = Date.now();
  await start(second, body("hold-locked"));
  await until(() => pending.get("hold-locked")?.length === 2);
  assert.ok(Date.now() - retryStart < 1000);
  await reply(pending.get("hold-locked")[1], "hold-locked", "delivered");
  assert.equal((await settle(second)).status, "delivered");
  await reply(pending.get("hold-locked")[0], "hold-locked", "queued");
  assert.equal((await settle(first)).status, "delivered");
  await start(second, body("hold-locked"));
  assert.equal((await settle(second)).status, "delivered");
  assert.equal(pending.get("hold-locked").length, 2);

  // Late replies cannot regress a newer accepted receipt or add a false error.
  for (const code of [200, 503, 400]) {
    const id = `hold-fallback-${code}`;
    await start(first, body(id));
    await until(() => pending.get(id)?.length === 1);
    await start(second, body(id));
    await until(() => pending.get(id)?.length === 2);
    await reply(pending.get(id)[1], id, "delivered");
    assert.equal((await settle(second)).status, "delivered");
    await reply(pending.get(id)[0], id, "queued", code);
    assert.equal((await settle(first)).status, "delivered");
    const value = await stored(first, id);
    assert.equal(value.status, "accepted");
    assert.equal(value.receipt.status, "delivered");
    assert.equal(value.error, undefined);
    assert.deepEqual(
      posts.filter((post) => post.id === id),
      [body(id), body(id)],
    );
  }

  for (const finalStatus of ["delivered", "uncertain"]) {
    const id = `hold-upgrade-${finalStatus}`;
    await start(first, body(id));
    await until(() => pending.get(id)?.length === 1);
    await start(second, body(id));
    await until(() => pending.get(id)?.length === 2);
    await reply(pending.get(id)[0], id, "queued");
    assert.equal((await settle(first)).status, "queued");
    await reply(pending.get(id)[1], id, finalStatus);
    assert.equal((await settle(second)).status, finalStatus);
    assert.equal((await stored(second, id)).receipt.status, finalStatus);
  }

  // The persistence callback cannot authorize reuse of an identity with new text.
  const mismatch = await first.evaluate(
    async (value) => {
      let persisted = false;
      try {
        await window.send.durableSend(value, [], () => {
          persisted = true;
        });
        return { persisted };
      } catch (error) {
        return { persisted, error: error.message };
      }
    },
    { ...body("hold-locked"), text: "Changed body" },
  );
  assert.equal(mismatch.persisted, false);
  assert.match(mismatch.error, /different content/);
  await first.evaluate(async () => {
    await window.storage.db.outbox.insert({
      id: "failed-before-send",
      seq: 0,
      payload: JSON.stringify({
        body: {
          id: "failed-before-send",
          room: "mobile-chat",
          text: "Rejected",
        },
        status: "failed",
        error: "Rejected previously",
        created: Date.now(),
      }),
    });
  });
  const failed = await first.evaluate(async () => {
    let persisted = false;
    try {
      await window.send.durableSend(
        { id: "failed-before-send", room: "mobile-chat", text: "Rejected" },
        [],
        () => {
          persisted = true;
        },
      );
      return { persisted };
    } catch (error) {
      return { persisted, error: error.message };
    }
  });
  assert.deepEqual(failed, { persisted: false, error: "Rejected previously" });

  // A real stalled fetch expires, preserves the request, and accepts an exact retry.
  const hung = body("hold-timeout");
  const timeoutStart = Date.now();
  await start(first, hung);
  await until(() => pending.get(hung.id)?.length === 1);
  const timeoutResult = await settle(first);
  assert.equal(timeoutResult.queued, true);
  assert.match(timeoutResult.error, /did not respond in time/);
  assert.ok(Date.now() - timeoutStart < 20000);
  assert.equal((await stored(first, hung.id)).attempted, true);
  await pending
    .get(hung.id)[0]
    .abort("failed")
    .catch(() => {});
  await start(second, hung);
  await until(() => pending.get(hung.id)?.length === 2);
  await reply(pending.get(hung.id)[1], hung.id, "delivered");
  assert.equal((await settle(second)).status, "delivered");
  assert.equal((await stored(second, hung.id)).error, undefined);
  assert.deepEqual(
    posts.filter((post) => post.id === hung.id),
    [hung, hung],
  );
  // Reconnect keeps same-chat order without making another chat wait for HTTP.
  await context.setOffline(true);
  const orderedFirst = { ...body("hold-order-first"), room: "slow-chat" };
  const orderedSecond = { ...body("hold-order-second"), room: "slow-chat" };
  const independent = { ...body("independent-chat-send"), room: "fast-chat" };
  for (const value of [orderedFirst, orderedSecond, independent]) {
    await start(first, value);
    assert.equal((await settle(first)).queued, true);
  }
  await context.setOffline(false);
  await first.evaluate(async () => {
    window.reactRoot = (
      await import("/@id/__x00__virtual:mobile-send-harness")
    ).mount();
  });
  await until(
    () =>
      pending.get(orderedFirst.id)?.length === 1 &&
      posts.some((post) => post.id === independent.id),
  );
  assert.equal(pending.has(orderedSecond.id), false);
  const continuationStart = Date.now();
  await reply(pending.get(orderedFirst.id)[0], orderedFirst.id, "delivered");
  await until(() => pending.get(orderedSecond.id)?.length === 1);
  assert.ok(
    Date.now() - continuationStart < 1500,
    "Next send does not wait for a poll",
  );
  await reply(pending.get(orderedSecond.id)[0], orderedSecond.id, "delivered");
  await until(
    async () => (await stored(first, orderedSecond.id)).status === "accepted",
  );
  await first.evaluate(() => window.reactRoot.unmount());
  assert.deepEqual(errors, []);
  console.log(
    "PASS: offline persistence, immediate callback, reconnect, cancellation boundary, concurrent tabs, late receipt/error guards, immutable IDs, stalled HTTP retry, ordered parallel chat recovery",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(temporary, { recursive: true, force: true });
}
