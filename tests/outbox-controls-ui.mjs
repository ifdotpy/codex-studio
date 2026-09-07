#!/usr/bin/env node
// Full React client and shared RxDB, isolated server. No live agent requests.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(repo, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const evidence = await mkdtemp(join(tmpdir(), "studio-outbox-controls-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  server,
  log = "";
fixture.stderr.on("data", (chunk) => {
  log += chunk;
});
async function until(check) {
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error("The expected outbox state did not arrive.");
}
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const target = `http://127.0.0.1:${port}`;
  const identity = await (await fetch(target + "/api/sync/identity")).json();
  server = await createServer({
    configFile: false,
    root: join(repo, "web"),
    cacheDir: join(evidence, "vite"),
    server: {
      host: "127.0.0.1",
      port: 0,
      proxy: {
        "/api": {
          target,
          changeOrigin: true,
          configure(proxy) {
            proxy.on("proxyReq", (request) =>
              request.setHeader("origin", target),
            );
          },
        },
      },
    },
  });
  await server.listen();
  const origin = server.resolvedUrls.local[0];
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const context = await browser.newContext({
    viewport: { width: 1440, height: 960 },
  });
  const errors = [],
    posts = [];
  let available = true,
    holdSession = null,
    holdPost = null;
  await context.route("**/api/sync/identity", (route) =>
    route.fulfill({
      status: available ? 200 : 503,
      json: available ? identity : { error: "Connection unavailable" },
    }),
  );
  await context.route("**/api/session", async (route) => {
    if (holdSession) return holdSession(route);
    return route.fallback();
  });
  await context.route("**/api/messages", async (route) => {
    assert.equal(route.request().method(), "POST");
    const body = route.request().postDataJSON();
    posts.push(body);
    if (holdPost) return holdPost(route);
    await route.fulfill({ json: { id: body.id, status: "accepted" } });
  });
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error" && message.text().includes("Encountered two children"))
      errors.push(message.text());
  });
  await page.goto(origin);
  await page.locator("#message").waitFor();
  await page.evaluate(async () => {
    window.outboxModule = await import("/src/sync/send.ts");
    window.syncModule = await import("/src/sync/client.ts");
    await window.syncModule.syncDatabase();
  });
  const entry = (text) => page.locator(".message").filter({ hasText: text });
  available = false;
  await page.locator("#message").fill("Cancel while offline");
  await page.locator("#send").click();
  await entry("Cancel while offline")
    .getByRole("button", { name: "Cancel send", exact: true })
    .click();
  await entry("Cancel while offline")
    .getByRole("status")
    .filter({ hasText: /^Cancelled$/ })
    .waitFor();
  assert.equal(posts.length, 0);
  available = true;
  await page.reload();
  await page.locator("#message").waitFor();
  await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
  await entry("Cancel while offline")
    .getByRole("status")
    .filter({ hasText: /^Cancelled$/ })
    .waitFor();
  assert.equal(posts.length, 0, "Cancellation survives reload and resume");

  await page
    .locator('input[type="file"]')
    .setInputFiles({
      name: "notes.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("Saved attachment"),
    });
  await page
    .getByRole("button", { name: "Remove notes.txt", exact: true })
    .waitFor();
  available = false;
  await page.locator("#message").fill("Edit while offline");
  await page.locator("#send").click();
  await entry("Edit while offline")
    .getByRole("button", { name: "Edit message", exact: true })
    .waitFor();
  await page.locator("#message").fill("Another draft");
  await entry("Edit while offline")
    .getByRole("button", { name: "Edit message", exact: true })
    .click();
  await entry("Edit while offline").getByRole("alert").waitFor();
  assert.equal(await page.locator("#message").inputValue(), "Another draft");
  await page.locator("#message").fill("");
  await entry("Edit while offline")
    .getByRole("button", { name: "Edit message", exact: true })
    .click();
  await page.waitForFunction(
    () => document.querySelector("#message").value === "Edit while offline",
  );
  await page
    .getByRole("button", { name: "Remove notes.txt", exact: true })
    .waitFor();
  await entry("Edit while offline")
    .getByRole("status")
    .filter({ hasText: /^Cancelled$/ })
    .waitFor();
  await page.locator("#message").fill("Corrected offline message");
  available = true;
  await page.locator("#send").click();
  await entry("Corrected offline message")
    .getByRole("status")
    .filter({ hasText: /^Sent$/ })
    .waitFor();
  assert.equal(posts.length, 1);
  assert.equal(posts[0].text, "Corrected offline message");
  assert.equal(posts[0].assets.length, 1);
  await page.evaluate(async () => {
    window.outboxModule = await import("/src/sync/send.ts");
    window.syncModule = await import("/src/sync/client.ts");
  });

  const second = await context.newPage();
  await second.route("**/empty-check", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: "<!doctype html><title>Second tab</title>",
    }),
  );
  await second.goto(origin + "empty-check");
  await second.evaluate(async () => {
    window.outboxModule = await import("/src/sync/send.ts");
    await (await import("/src/sync/client.ts")).syncDatabase();
  });
  const room = posts[0].room;
  const beforeRaces = posts.length;
  const pendingSessions = [];
  holdSession = (route) => {
    pendingSessions.push(route);
  };
  await page.evaluate((room) => {
    window.raceResult = window.outboxModule.durableSend({
      id: "cancel-before-post",
      room,
      text: "Never post this",
    });
  }, room);
  await until(() =>
    page.evaluate(async () => {
      const { db } = await window.syncModule.syncDatabase();
      return !!(await db.outbox.findOne("cancel-before-post").exec());
    }),
  );
  await second.evaluate(() =>
    window.outboxModule.changeOutbox("cancel-before-post", "cancel"),
  );
  holdSession = null;
  for (const route of pendingSessions) await route.fallback();
  const cancelled = await page.evaluate(() => window.raceResult);
  assert.equal(cancelled.status, "cancelled");
  assert.equal(
    posts.length,
    beforeRaces,
    "A second tab cancels before the HTTP claim",
  );

  let postRoute;
  holdPost = (route) => {
    postRoute = route;
  };
  const body = {
    id: "lost-response-pause",
    room,
    text: "Keep the immutable request",
    assets: [],
    delivery: "queue",
  };
  await page.evaluate((body) => {
    window.raceResult = window.outboxModule.durableSend(body);
  }, body);
  await until(() =>
    page.evaluate(async () => {
      const { db } = await window.syncModule.syncDatabase();
      const doc = await db.outbox.findOne("lost-response-pause").exec();
      return doc && JSON.parse(doc.payload).attempted;
    }),
  );
  const forbidden = await second.evaluate(async () => {
    try {
      await window.outboxModule.changeOutbox("lost-response-pause", "cancel");
      return "";
    } catch (error) {
      return error.message;
    }
  });
  assert.match(forbidden, /Delivery may have started/);
  await second.evaluate(() =>
    window.outboxModule.changeOutbox("lost-response-pause", "pause"),
  );
  const deadline = Date.now() + 5000;
  while (!postRoute && Date.now() < deadline)
    await new Promise((resolve) => setTimeout(resolve, 20));
  assert.ok(postRoute);
  await postRoute.abort("failed");
  holdPost = null;
  const paused = await page.evaluate(() => window.raceResult);
  assert.equal(paused.status, "paused");
  await entry("Keep the immutable request")
    .getByRole("button", { name: "Resume retries", exact: true })
    .waitFor();
  const pausedCount = posts.length;
  await page.reload();
  await entry("Keep the immutable request")
    .getByRole("button", { name: "Resume retries", exact: true })
    .waitFor();
  await page.evaluate(() => window.dispatchEvent(new Event("pageshow")));
  assert.equal(posts.length, pausedCount);
  await entry("Keep the immutable request")
    .getByRole("button", { name: "Resume retries", exact: true })
    .click();
  await entry("Keep the immutable request")
    .getByRole("status")
    .filter({ hasText: /^Sent$/ })
    .waitFor();
  assert.deepEqual(
    posts.slice(-2),
    [body, body],
    "Explicit resume retries the same body and ID",
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS: offline cancel, edit with attachments, existing draft guard, reload, cross-tab cancellation boundary, pause after lost response, immutable resume",
  );
} finally {
  await browser?.close();
  await server?.close();
  fixture.kill("SIGTERM");
  await rm(join(evidence, "vite"), { recursive: true, force: true });
}
