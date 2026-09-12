#!/usr/bin/env node
// Cache boundaries use the actual module and isolated browser storage.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
const root = join(import.meta.dirname, "../web");
const require = createRequire(join(root, "package.json"));
const { chromium, webkit } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const cacheDir = await mkdtemp(join(tmpdir(), "studio-progress-cache-"));
const entry = join(root, "progress-cache-fixture.ts");
const server = await createServer({
  configFile: false,
  root,
  cacheDir,
  server: { host: "127.0.0.1", port: 0 },
  optimizeDeps: { noDiscovery: true },
  plugins: [
    {
      name: "progress-cache-fixture",
      configureServer(server) {
        server.middlewares.use("/check", (_request, response) => {
          response.setHeader("Content-Type", "text/html");
          response.end(
            '<script type="module" src="/progress-cache-fixture.ts"></script>',
          );
        });
      },
      resolveId(id) {
        if (id === "/progress-cache-fixture.ts") return entry;
      },
      load(id) {
        if (id === entry)
          return "import * as cache from '/src/components/progressCache.ts'; window.progressCache = cache;";
      },
    },
  ],
});
let browser;
try {
  await server.listen();
  browser = await browserType.launch({
    headless: true,
    ...(browserType === chromium
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    const original = window.fetch;
    window.progressFixture = {
      mode: "ok",
      markdown: "Saved progress",
      revision: "one",
      calls: 0,
      held: [],
    };
    window.fetch = (input, options) => {
      const url = new URL(String(input), location.href);
      if (url.pathname !== "/api/panel") return original(input, options);
      const fixture = window.progressFixture;
      fixture.calls++;
      const value = {
        agent: url.searchParams.get("agent"),
        format: "markdown",
        markdown: fixture.markdown,
        revision: fixture.revision,
        path: "/fixture/PROGRESS.md",
      };
      const reply = () =>
        new Response(JSON.stringify(value), {
          headers: { "Content-Type": "application/json" },
        });
      if (fixture.mode === "hold")
        return new Promise((resolve) =>
          fixture.held.push(() => resolve(reply())),
        );
      if (fixture.mode === "error")
        return Promise.reject(new TypeError("Disconnected"));
      return Promise.resolve(reply());
    };
  });
  const origin = `http://127.0.0.1:${server.httpServer.address().port}/check`;
  await page.goto(origin);
  await page.waitForFunction(() => !!window.progressCache);
  await page.evaluate(async () => {
    await progressCache.readProgress("state-a", "first");
    if (progressCache.peekProgress("state-b", "first") !== null)
      throw Error("State scope leaked");
    if (progressCache.peekProgress("state-a", "other") !== null)
      throw Error("Agent scope leaked");
  });
  await page.reload();
  await page.waitForFunction(() => !!window.progressCache);
  assert.equal(
    await page.evaluate(
      () => progressCache.peekProgress("state-a", "first")?.markdown,
    ),
    "Saved progress",
  );
  assert.equal(await page.evaluate(() => progressFixture.calls), 0);
  console.log("PASS synchronous durable recovery and state/agent isolation");

  const shared = await page.evaluate(async () => {
    progressFixture.mode = "hold";
    const controller = new AbortController();
    const first = progressCache
      .readProgress("state-a", "shared", controller.signal)
      .catch((error) => error.name);
    const second = progressCache.readProgress("state-a", "shared");
    const calls = progressFixture.calls;
    controller.abort();
    progressFixture.held.shift()();
    return { first: await first, second: (await second).markdown, calls };
  });
  assert.deepEqual(shared, {
    first: "AbortError",
    second: "Saved progress",
    calls: 1,
  });
  console.log("PASS shared fetch survives cancellation of one reader");

  const expiry = await page.evaluate(async () => {
    progressFixture.mode = "hold";
    const first = progressCache
      .readProgress("state-a", "expired")
      .catch((error) => error.name);
    const original = Date.now;
    Date.now = () => original() + 9000;
    progressFixture.mode = "ok";
    progressFixture.markdown = "Fresh after resume";
    const next = await progressCache.readProgress("state-a", "expired");
    Date.now = original;
    progressFixture.held.shift()();
    await Promise.resolve();
    return {
      error: await first,
      next: next.markdown,
      cached: progressCache.peekProgress("state-a", "expired").markdown,
    };
  });
  assert.deepEqual(expiry, {
    error: "NetworkTimeoutError",
    next: "Fresh after resume",
    cached: "Fresh after resume",
  });
  console.log(
    "PASS expired shared request is replaced and late reply cannot overwrite it",
  );

  const oversized = await page.evaluate(async () => {
    progressFixture.markdown = "Small";
    await progressCache.readProgress("state-a", "large");
    progressFixture.markdown = "x".repeat(600000);
    const read = await progressCache.readProgress("state-a", "large");
    return {
      size: read.markdown.length,
      stored: localStorage.getItem(
        "codex-progress-cache:" +
          progressCache.progressScope("state-a", "large"),
      ),
    };
  });
  assert.deepEqual(oversized, { size: 600000, stored: null });
  console.log("PASS oversized authoritative content survives cache refusal");

  const bounded = await page.evaluate(async () => {
    localStorage.setItem("fixture-important-draft", "Do not remove");
    progressFixture.markdown = "x".repeat(6000);
    for (let index = 0; index < 40; index++)
      await progressCache.readProgress("state-a", `bounded-${index}`);
    const keys = Object.keys(localStorage).filter((key) =>
      key.startsWith("codex-progress-cache:"),
    );
    const bytes = keys.reduce(
      (total, key) => total + localStorage.getItem(key).length * 2,
      0,
    );
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (key.startsWith("codex-progress-cache:"))
        throw new DOMException("Full", "QuotaExceededError");
      return original.call(this, key, value);
    };
    progressFixture.markdown = "Memory remains available";
    await progressCache.readProgress("state-a", "quota");
    Storage.prototype.setItem = original;
    return {
      count: keys.length,
      bytes,
      memory: progressCache.peekProgress("state-a", "quota").markdown,
      draft: localStorage.getItem("fixture-important-draft"),
    };
  });
  assert.ok(bounded.count <= 32);
  assert.ok(bounded.bytes <= 256 * 1024);
  assert.equal(bounded.memory, "Memory remains available");
  assert.equal(bounded.draft, "Do not remove");
  console.log(
    "PASS bounded cache and quota failure preserve drafts and memory",
  );

  await page.evaluate(async () => {
    progressFixture.markdown = "";
    progressFixture.revision = null;
    await progressCache.readProgress("state-a", "quota");
  });
  assert.equal(
    await page.evaluate(
      () => progressCache.peekProgress("state-a", "quota").markdown,
    ),
    "",
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS ${browserType.name()}: empty file clears earlier cached content; six cache boundaries`,
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cacheDir, { recursive: true, force: true });
}
