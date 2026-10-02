import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(new URL("../web/package.json", import.meta.url));
const { chromium } = require("playwright-core");
const { createServer } = await import(
  new URL("../web/node_modules/vite/dist/node/index.js", import.meta.url)
);
const workspaceId = "e".repeat(32);
const attempts = new Map();
const streams = new Set();
const server = await createServer({
  configFile: false,
  root: fileURLToPath(new URL("../web", import.meta.url)),
  server: { host: "127.0.0.1", port: 0 },
});
server.middlewares.stack.unshift({
  route: "",
  handle(req, res, next) {
    const url = new URL(req.url, "http://localhost");
    const json = (value, status = 200) => {
      res.writeHead(status, { "Content-Type": "application/json" });
      res.end(JSON.stringify(value));
    };
    if (url.pathname === "/check") res.end("<!doctype html>");
    else if (url.pathname === "/api/sync/identity") json({ workspaceId });
    else if (url.pathname === "/api/sync/protocol")
      json({ protocolVersion: 1, capabilities: ["streamChanges"] });
    else if (url.pathname === "/api/sync/stream") {
      res.writeHead(200, { "Content-Type": "text/event-stream" });
      res.write(": connected\n\n");
      streams.add(res);
      res.on("close", () => streams.delete(res));
    } else if (url.pathname === "/api/sync/pull") {
      const scope = url.searchParams.get("scope");
      const kind = scope.slice(11);
      const count = (attempts.get(kind) || 0) + 1;
      attempts.set(kind, count);
      if (["429", "503"].includes(kind) && count === 1)
        return json({ error: "Temporary failure" }, Number(kind));
      if (kind === "403") return json({ error: "Access denied" }, 403);
      json({
        workspaceId: kind === "workspace" ? "f".repeat(32) : workspaceId,
        documents: [
          {
            id: scope,
            seq: 1,
            payload:
              kind === "decode" && count === 1
                ? "invalid-json"
                : JSON.stringify({ items: [] }),
          },
        ],
        checkpoint: { seq: 1 },
      });
    } else next();
  },
});
await server.listen();
const browser = await chromium.launch({
  executablePath:
    process.env.CHROME_BIN ||
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  headless: true,
});
try {
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(`http://127.0.0.1:${server.httpServer.address().port}/check`);
  await page.evaluate(async () => {
    window.client = await import("/src/sync/client.ts");
    const { NetworkTimeoutError, ApiError } = await import("/src/api.ts");
    const originalFetch = window.fetch;
    window.transientAttempts = {};
    window.fetch = (...args) => {
      const url = new URL(String(args[0]), location.href);
      const kind = url.searchParams.get("scope")?.slice(11);
      if (
        url.pathname === "/api/sync/pull" &&
        ["timeout", "network", "408"].includes(kind)
      ) {
        window.transientAttempts[kind] =
          (window.transientAttempts[kind] || 0) + 1;
        if (window.transientAttempts[kind] === 1)
          return new Promise((_, reject) =>
            setTimeout(
              () =>
                reject(
                  kind === "timeout"
                    ? new NetworkTimeoutError()
                    : kind === "408"
                      ? new ApiError("Temporary failure", 408)
                      : new TypeError("Failed to fetch"),
                ),
              40,
            ),
          );
      }
      return originalFetch(...args);
    };
  });
  for (const kind of ["timeout", "network", "408", "429", "503"]) {
    await page.evaluate((kind) => {
      window.states = [];
      window.stop = window.client.subscribeProjection(
        `transcript:${kind}`,
        () => {},
        (error) => window.states.push(error ? String(error) : "live"),
      );
    }, kind);
    try {
      await page.waitForFunction(
        () => window.states.length >= 2 && window.states.at(-1) === "live",
        undefined,
        { timeout: 5000 },
      );
    } catch (error) {
      console.error(
        kind,
        await page.evaluate(() => ({
          states: window.states,
          transientAttempts: window.transientAttempts,
        })),
        attempts.get(kind),
      );
      throw error;
    }
    assert.ok(
      await page.evaluate(() =>
        window.states.some((state) => state !== "live"),
      ),
      `${kind} must fail before it recovers without a resume event.`,
    );
    if (["timeout", "network", "408"].includes(kind))
      assert.equal(
        await page.evaluate((kind) => window.transientAttempts[kind], kind),
        2,
      );
    else assert.equal(attempts.get(kind), 2);
    await page.evaluate(() => window.stop());
  }
  for (const kind of ["403", "workspace"]) {
    await page.evaluate((kind) => {
      window.states = [];
      window.stop = window.client.subscribeProjection(
        `transcript:${kind}`,
        () => {},
        (error) => window.states.push(error ? String(error) : "live"),
      );
    }, kind);
    await page.waitForFunction(() => window.states.length > 0);
    await new Promise((resolve) => setTimeout(resolve, 600));
    assert.equal(attempts.get(kind), 1, `${kind} must not authorize a retry.`);
    assert.ok(await page.evaluate(() => !window.states.includes("live")));
    await page.evaluate(() => window.stop());
  }
  await page.evaluate(() => {
    window.states = [];
    window.stop = window.client.subscribeProjection(
      "transcript:decode",
      () => {},
      (error) => window.states.push(error ? String(error) : "live"),
    );
  });
  await page.waitForFunction(() =>
    window.states.some((state) => state !== "live"),
  );
  for (const response of streams)
    response.write(
      `data: ${JSON.stringify({
        protocol: 2,
        workspaceId,
        generations: { state: 2, drafts: 2, transcripts: 2 },
      })}\n\n`,
    );
  await page.waitForFunction(() => window.states.at(-1) === "live");
  assert.equal(
    attempts.get("decode"),
    2,
    "A new stream hint must recover a malformed projection response.",
  );
  await page.evaluate(() => window.stop());
  assert.deepEqual(errors, []);
  console.log("sync read retry browser contract passed");
} finally {
  await browser.close();
  for (const response of streams) response.end();
  await server.close();
}
