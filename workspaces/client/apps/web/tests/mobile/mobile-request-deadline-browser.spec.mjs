import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { join } from "node:path";
import { test, browserExecutablePath } from "../playwright.mjs";

const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
const require = createRequire(
  join(repo, "workspaces/client/apps/web/package.json"),
);
const { chromium, webkit } = require("playwright-core");
const webkitPath = webkit.executablePath();

async function runDeadlineChecks(name, engine, executablePath) {
  // Browser request deadlines still expire after mobile JavaScript suspension.
  const { createServer } = await import(require.resolve("vite"));
  const server = await createServer({
    configFile: false,
    root: join(repo, "workspaces/client/apps/web"),
    server: { host: "127.0.0.1", port: 0 },
  });
  await server.listen();
  try {
    const browser = await engine.launch({ headless: true, executablePath });
    try {
      const page = await browser.newPage();
      await page.route("**/deadline-fixture", (r) =>
        r.fulfill({
          contentType: "text/html",
          body: "<!doctype html><title>Deadline fixture</title>",
        }),
      );
      const held = new Map();
      await page.route("**/api/held-*", (r) =>
        held.set(new URL(r.request().url()).pathname, r),
      );
      await page.goto(server.resolvedUrls.local[0] + "deadline-fixture");
      await page.evaluate(async () => {
        const { api, syncApi } = await import("/src/api.ts");
        window.outcomes = {};
        window.start = Date.now();
        const realNow = Date.now;
        window.advance = 0;
        Date.now = () => realNow() + window.advance;
        const record = (key, p) =>
          p.then(
            () => (window.outcomes[key] = "success"),
            (e) => (window.outcomes[key] = e.name),
          );
        record("read", api("/api/held-read"));
        record("durable", syncApi("/api/held-durable", { id: "fixed-id" }));
        record("ordinary", api("/api/held-ordinary", { action: "one-time" }));
      });
      await page.waitForTimeout(100);
      assert.equal(held.size, 3);
      await page.evaluate(() => {
        window.advance = 16000;
        window.dispatchEvent(new Event("pageshow"));
      });
      await page.waitForFunction(
        () => window.outcomes.read && window.outcomes.durable,
      );
      assert.deepEqual(await page.evaluate(() => window.outcomes), {
        read: "NetworkTimeoutError",
        durable: "NetworkTimeoutError",
      });
      await held.get("/api/held-ordinary").fulfill({ json: { ok: true } });
      await page.waitForFunction(() => window.outcomes.ordinary === "success");
      await page.evaluate(async () => {
        const { api } = await import("/src/api.ts");
        const record = (key, promise) =>
          promise.then(
            () => (window.outcomes[key] = "success"),
            (error) => (window.outcomes[key] = error.name),
          );
        window.cancelWithDeadline = new AbortController();
        window.cancelOnly = new AbortController();
        const alreadyAborted = new AbortController();
        alreadyAborted.abort();
        record(
          "cancelledRead",
          api("/api/held-cancel-read", undefined, {
            signal: window.cancelWithDeadline.signal,
            timeoutMs: 15000,
          }),
        );
        record(
          "cancelledPost",
          api(
            "/api/held-cancel-post",
            {},
            {
              signal: window.cancelOnly.signal,
            },
          ),
        );
        record(
          "alreadyAborted",
          api(
            "/api/held-already-aborted",
            {},
            {
              signal: alreadyAborted.signal,
              timeoutMs: 15000,
            },
          ),
        );
      });
      await page.waitForTimeout(100);
      assert(held.has("/api/held-cancel-read"));
      assert(held.has("/api/held-cancel-post"));
      await page.evaluate(() => {
        window.cancelWithDeadline.abort();
        window.cancelOnly.abort();
      });
      await page.waitForFunction(
        () => window.outcomes.cancelledRead && window.outcomes.cancelledPost,
      );
      assert.deepEqual(
        await page.evaluate(() => ({
          read: window.outcomes.cancelledRead,
          post: window.outcomes.cancelledPost,
          beforeStart: window.outcomes.alreadyAborted,
        })),
        { read: "AbortError", post: "AbortError", beforeStart: "AbortError" },
      );
      assert(!held.has("/api/held-already-aborted"));
      console.log(
        `${name}: deadlines release on resume; ordinary mutations stay intact; external cancellation works with or without a deadline`,
      );
    } finally {
      await browser.close();
    }
  } finally {
    await server.close();
  }
}

test("Mobile request deadline (Chromium)", async () => {
  test.setTimeout(180_000);
  await runDeadlineChecks("chromium", chromium, browserExecutablePath);
});

test("Mobile request deadline (WebKit)", async () => {
  test.setTimeout(180_000);
  test.skip(
    !existsSync(webkitPath),
    `WebKit runtime is not installed at ${webkitPath}`,
  );
  await runDeadlineChecks("webkit", webkit, webkitPath);
});
