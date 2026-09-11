// Real App, Conversation, and RxDB. Only an isolated fixture receives writes.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(root, "web/package.json"));
const { chromium, webkit } = require("playwright-core");
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const { createServer } = await import(require.resolve("vite"));
const temporary = await mkdtemp(join(tmpdir(), "studio-mobile-send-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(root, "tests/simple-ui-fixture.py"), temporary],
  {
    stdio: ["pipe", "pipe", "pipe"],
  },
);
let browser,
  server,
  log = "";
fixture.stderr.on("data", (chunk) => {
  log += chunk;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const target = `http://127.0.0.1:${port}`;
  server = await createServer({
    configFile: false,
    root: join(root, "web"),
    cacheDir: join(temporary, "vite"),
    server: {
      host: "127.0.0.1",
      port: 0,
      hmr: false,
      proxy: {
        "/api": {
          target,
          changeOrigin: true,
          configure(proxy) {
            proxy.on("proxyReq", (request) =>
              request.setHeader("Origin", target),
            );
          },
        },
      },
    },
  });
  await server.listen();
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
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  const errors = [],
    posts = [],
    pending = [];
  let replyStatus = "hold";
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/messages", (route) => {
    const body = route.request().postDataJSON();
    posts.push(body);
    if (replyStatus === "hold") pending.push(route);
    else return route.fulfill({ json: { id: body.id, status: replyStatus } });
  });
  await page.goto(server.resolvedUrls.local[0]);
  const input = page.locator("#message"),
    send = page.locator("#send");
  await input.waitFor();
  await page.evaluate(async () => {
    window.storage = await (await import("/src/sync/client.ts")).syncDatabase();
    window.queued = async () =>
      (await window.storage.db.outbox.find().exec()).map((doc) => ({
        id: doc.id,
        ...JSON.parse(doc.getLatest().payload),
      }));
  });
  const until = async (check) => {
    const deadline = Date.now() + 15000;
    while (!(await check())) {
      if (Date.now() > deadline)
        throw new Error("Expected mobile send state did not arrive");
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
  };
  await page
    .locator('input[type="file"]')
    .setInputFiles(
      Array.from({ length: 9 }, (_, index) => ({
        name: `too-many-${index}.txt`,
        mimeType: "text/plain",
        buffer: Buffer.from("Keep the refused selection"),
      })),
    );
  await page
    .getByText("Attach at most eight files.", { exact: true })
    .first()
    .waitFor();
  assert.equal(
    await page
      .locator('input[type="file"]')
      .evaluate((input) => input.files.length),
    9,
    "Admission refusal must not clear uncommitted files",
  );
  await page.locator('input[type="file"]').setInputFiles({
    name: "first.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("First send attachment"),
  });
  await page
    .getByRole("button", { name: "Remove first.txt", exact: true })
    .waitFor();
  // A reload can happen after Send but before IndexedDB commits the outbox.
  await page.evaluate(() => {
    window.storage.db.outbox.insert = () => {
      window.insertStarted = true;
      return new Promise(() => {});
    };
  });
  await input.fill("Preserve this uncommitted send");
  await send.click();
  await page.waitForFunction(() => window.insertStarted);
  assert.equal(await input.inputValue(), "Preserve this uncommitted send");
  assert.equal(
    await page
      .getByRole("button", { name: "Remove first.txt", exact: true })
      .count(),
    1,
  );
  assert.equal(posts.length, 0, "No HTTP send before local persistence");
  await page.reload();
  await input.waitFor();
  await until(
    async () => (await input.inputValue()) === "Preserve this uncommitted send",
  );
  await page
    .getByRole("button", { name: "Remove first.txt", exact: true })
    .waitFor();
  assert.equal(
    posts.length,
    0,
    "Reload does not invent a send for an uncommitted draft",
  );
  await page.evaluate(async () => {
    window.storage = await (await import("/src/sync/client.ts")).syncDatabase();
    window.queued = async () =>
      (await window.storage.db.outbox.find().exec()).map((doc) => ({
        id: doc.id,
        ...JSON.parse(doc.getLatest().payload),
      }));
  });
  await input.fill("Same intentional message");
  await send.click();
  await until(() => posts.length === 1);
  assert.equal(posts[0].assets.length, 1);
  await input.fill("Same intentional message");
  const nextStart = Date.now();
  await send.click();
  await until(() =>
    page.evaluate(async () => (await window.queued()).length === 2),
  );
  assert.ok(
    Date.now() - nextStart < 1500,
    "Second message persists before the first HTTP reply",
  );
  assert.equal(await input.inputValue(), "");
  assert.equal(posts.length, 1, "Same-chat HTTP requests retain their order");
  const initial = await page.evaluate(() => window.queued());
  assert.notEqual(
    initial[0].id,
    initial[1].id,
    "Two intentional sends have distinct identities",
  );
  assert.equal(initial[0].body.text, initial[1].body.text);
  replyStatus = "delivered";
  await pending
    .shift()
    .fulfill({ status: 400, json: { error: "First message rejected" } });
  await until(() => posts.length === 2);
  await until(() =>
    page.evaluate(async () =>
      (await window.queued()).some(
        (entry) =>
          entry.body.assets.length === 0 && entry.status === "accepted",
      ),
    ),
  );
  assert.equal(
    await input.inputValue(),
    "",
    "An older failure cannot restore a newer completed draft",
  );
  assert.equal(
    await page
      .getByRole("button", { name: "Remove first.txt", exact: true })
      .count(),
    0,
    "An older failure cannot restore attachments after a later submission",
  );
  assert.equal(posts[1].assets.length, 0);

  // Hold the next IndexedDB insert. An older HTTP failure must not release the
  // newer send's lock before that send has a durable record.
  replyStatus = "hold";
  await input.fill("Older in-flight send");
  await send.click();
  await until(() => posts.length === 3);
  await page.evaluate(() => {
    const insert = window.storage.db.outbox.insert.bind(
      window.storage.db.outbox,
    );
    window.storage.db.outbox.insert = async (...args) => {
      if (JSON.parse(args[0].payload).body.text === "Newer durable write") {
        window.insertStarted = true;
        await new Promise((resolve) => {
          window.releaseInsert = resolve;
        });
      }
      return insert(...args);
    };
  });
  await input.fill("Newer durable write");
  await send.click();
  await page.waitForFunction(() => window.insertStarted);
  await pending
    .shift()
    .fulfill({ status: 400, json: { error: "Older request rejected" } });
  await until(() =>
    page.evaluate(async () =>
      (await window.queued()).some(
        (entry) =>
          entry.body.text === "Older in-flight send" &&
          entry.status === "failed",
      ),
    ),
  );
  await input.fill("Unsent draft after the click");
  assert.equal(
    await send.isDisabled(),
    true,
    "Older finally cannot release a newer unpersisted send",
  );
  replyStatus = "delivered";
  await page.evaluate(() => window.releaseInsert());
  await until(() => posts.length === 4);
  await until(() =>
    page.evaluate(async () =>
      (await window.queued()).some(
        (entry) =>
          entry.body.text === "Newer durable write" &&
          entry.status === "accepted",
      ),
    ),
  );
  assert.equal(await input.inputValue(), "Unsent draft after the click");
  assert.equal(await send.isEnabled(), true);

  // A latest unconfirmed send still restores its own draft and identity.
  replyStatus = "uncertain";
  await input.fill("Latest unconfirmed message");
  await send.click();
  await page.waitForFunction(
    () =>
      document.querySelector("#message").value === "Latest unconfirmed message",
  );
  await until(() => posts.length === 5);
  const last = posts.at(-1);
  await send.click();
  await page.waitForFunction(
    () =>
      document.querySelector("#message").value === "Latest unconfirmed message",
  );
  assert.equal(
    posts.length,
    5,
    "Unconfirmed retry retains the prior identity and does not replay HTTP",
  );
  const lastEntries = await page.evaluate(() => window.queued());
  assert.equal(
    lastEntries.filter((entry) => entry.body.text === last.text).length,
    1,
  );
  assert.equal(
    lastEntries.find((entry) => entry.body.text === last.text).id,
    last.id,
  );
  await page.context().setOffline(true);
  await input.fill("Mobile offline message");
  await send.click();
  await until(() =>
    page.evaluate(async () =>
      (await window.queued()).some(
        (entry) =>
          entry.body.text === "Mobile offline message" &&
          entry.status === "queued",
      ),
    ),
  );
  assert.equal(await input.inputValue(), "");
  assert.equal(posts.length, 5);
  replyStatus = "delivered";
  await page.context().setOffline(false);
  await until(() => posts.length === 6);
  await until(() =>
    page.evaluate(async () =>
      (await window.queued()).some(
        (entry) =>
          entry.body.text === "Mobile offline message" &&
          entry.status === "accepted",
      ),
    ),
  );
  assert.equal(
    posts.filter((post) => post.text === "Mobile offline message").length,
    1,
  );
  // An old browser receipt can still refer to a message already rejected by
  // the outbox. A reload must not trap every explicit retry on that failed ID.
  const stateDir = (await (await fetch(target + "/api/state")).json()).stateDir;
  const failedBody = {
    id: "old-failed-after-reload",
    room: posts[0].room,
    text: "Retry a stored terminal failure",
    assets: [],
    delivery: "queue",
  };
  const pendingKey = `studio-pending-sends:${stateDir}`;
  await page.evaluate(
    async ({ body, pendingKey }) => {
      await window.storage.db.outbox.insert({
        id: body.id,
        seq: 0,
        payload: JSON.stringify({
          body,
          status: "failed",
          error: "Old stored rejection",
          created: Date.now(),
          displayPending: true,
          attachments: [],
        }),
      });
      localStorage.setItem(pendingKey, JSON.stringify({ [body.room]: body }));
    },
    { body: failedBody, pendingKey },
  );
  await page.reload();
  await input.waitFor();
  await input.fill(failedBody.text);
  const beforeExplicitRetry = posts.length;
  await send.click();
  await page.waitForFunction(
    ({ pendingKey, room }) =>
      !JSON.parse(localStorage.getItem(pendingKey) || "{}")[room],
    { pendingKey, room: failedBody.room },
  );
  assert.equal(posts.length, beforeExplicitRetry);
  await page.waitForFunction(
    (text) => document.querySelector("#message").value === text,
    failedBody.text,
  );
  await send.click();
  await until(() => posts.length === beforeExplicitRetry + 1);
  assert.notEqual(posts.at(-1).id, failedBody.id);
  assert.equal(posts.at(-1).text, failedBody.text);

  // A second tab can retry while the first tab has a committed but stalled
  // response. The real HTTP handler and SQLite retain one logical message.
  const snapshot = await (await fetch(target + "/api/state")).json();
  const other = snapshot.runtime.agents.find(
    (agent) => agent.name === "Other project",
  );
  const realBody = {
    id: crypto.randomUUID(),
    room: other.id,
    text: "One real message across two mobile tabs",
    assets: [],
    delivery: "queue",
  };
  await page.unroute("**/api/messages");
  let committed = false,
    heldResponse;
  const realPosts = [];
  await page.context().route("**/api/messages", async (route) => {
    const request = route.request().postDataJSON();
    realPosts.push(request);
    const response = await route.fetch();
    assert.equal(response.status(), 200);
    if (realPosts.length === 1) {
      committed = true;
      heldResponse = route;
    } else await route.fulfill({ response });
  });
  await page.evaluate(async (body) => {
    window.httpSend = (await import("/src/sync/send.ts")).durableSend(body);
  }, realBody);
  await until(() => committed);
  const peer = await page.context().newPage();
  await peer.route("**/send-peer", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Second mobile tab</title>",
    }),
  );
  await peer.goto(server.resolvedUrls.local[0] + "send-peer");
  const peerResult = await peer.evaluate(
    async (body) => (await import("/src/sync/send.ts")).durableSend(body),
    realBody,
  );
  assert.equal(peerResult.id, realBody.id);
  assert.ok(realPosts.length >= 2);
  assert.ok(
    realPosts.every(
      (request) => JSON.stringify(request) === JSON.stringify(realBody),
    ),
  );
  await heldResponse.abort("failed");
  assert.equal((await page.evaluate(() => window.httpSend)).id, realBody.id);
  await page.reload();
  await input.waitFor();
  const counts = JSON.parse(
    execFileSync(
      "python3",
      [
        "-c",
        'import json,sqlite3,sys; db=sqlite3.connect(sys.argv[1]); print(json.dumps([db.execute("SELECT COUNT(*) FROM runtime_events WHERE id=?",(sys.argv[2],)).fetchone()[0],db.execute("SELECT COUNT(*) FROM messages WHERE id=?",(sys.argv[2],)).fetchone()[0]]))',
        join(temporary, "canvas.sqlite3"),
        realBody.id,
      ],
      { encoding: "utf8" },
    ),
  );
  assert.deepEqual(counts, [1, 1]);
  assert.deepEqual(errors, []);
  console.log(
    "PASS: draft and attachments survive reload before outbox commit, responsive mobile composer, ordered distinct sends, late error draft/attachment guard, late finally lock guard, safe uncertain retry, offline reconnect, real cross-tab HTTP deduplication",
  );
} finally {
  await browser?.close();
  await server?.close();
  fixture.stdin.end();
  await new Promise((resolve) => {
    fixture.once("exit", resolve);
    setTimeout(() => {
      fixture.kill("SIGTERM");
      resolve();
    }, 3000).unref();
  });
  await rm(join(temporary, "vite"), { recursive: true, force: true });
}
