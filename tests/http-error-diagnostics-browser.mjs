import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const cache = await mkdtemp(join(tmpdir(), "studio-http-errors-"));
const server = await createServer({
  configFile: false,
  cacheDir: cache,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
});
let browser;
try {
  await server.listen();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage();
  await page.route("**/check", (r) =>
    r.fulfill({ contentType: "text/html", body: "<!doctype html>" }),
  );
  await page.goto(server.resolvedUrls.local[0] + "check");
  let calls = 0;
  const cases = [
    {
      status: 429,
      body: {
        error: {
          message: { message: "Provider quota exhausted" },
          code: "quota",
          details: { reset: 123 },
        },
        requestId: "exact-request",
      },
      message: "Provider quota exhausted",
    },
    { status: 401, body: { error: "Sign in again" }, message: "Sign in again" },
    {
      status: 403,
      body: { message: "Permission denied", details: { operation: "read" } },
      message: "Permission denied",
    },
    { status: 409, body: "Request conflict", message: "Request conflict" },
    { status: 500, body: null, message: "Request failed (500)" },
  ];
  for (const fixture of cases) {
    await page.route("**/api/error-fixture", (route) => {
      calls++;
      assert.equal(route.request().method(), "GET");
      return route.fulfill({ status: fixture.status, json: fixture.body });
    });
    const result = await page.evaluate(async () => {
      const { api, ApiError, errorText } = await import("/src/api.ts");
      try {
        await api("/api/error-fixture");
        return null;
      } catch (error) {
        return {
          apiError: error instanceof ApiError,
          status: error.status,
          message: errorText(error),
          details: error.details,
        };
      }
    });
    assert.deepEqual(result, {
      apiError: true,
      status: fixture.status,
      message: fixture.message,
      details: fixture.body,
    });
    await page.unroute("**/api/error-fixture");
  }
  assert.equal(calls, cases.length, "Formatting does not retry requests");
  console.log(
    "PASS: structured/string/null HTTP failures retain status and complete response details; no automatic retries",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(cache, { recursive: true, force: true });
}
