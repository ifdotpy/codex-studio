import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
  expect,
} from "../playwright.ts";

test("sync-read-retry-browser", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const workspaceId = "e".repeat(32);
  const attempts = new Map();
  const streams = new Set();
  let revision = 0;
  const invalidate = (agentId) => {
    const resource = { kind: "transcript", agentId };
    for (const stream of streams) {
      if (
        !stream.resources.some(
          (entry) => JSON.stringify(entry) === JSON.stringify(resource),
        )
      )
        continue;
      revision++;
      stream.response.write(
        `event: resources\ndata: ${JSON.stringify({
          protocol: 4,
          workspaceId,
          epoch: "fixture-epoch",
          revision,
          reason: "change",
          resources: [resource],
          resourceVersions: [{ resource, revision }],
        })}\n\n`,
      );
    }
  };
  const server = await createServer({
    configFile: false,
    optimizeDeps: { noDiscovery: true },
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.stack.unshift({
    route: "",
    handle(req, res, next) {
      const url = new URL(req.url, "http://localhost");
      const json = (value, status = 200) => {
        res.writeHead(
          status,
          apiSchemaHeaders({ "Content-Type": "application/json" }),
        );
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") res.end("<!doctype html>");
      else if (url.pathname === "/api/sync/identity")
        json(syncIdentityFixture(workspaceId));
      else if (url.pathname === "/api/sync/protocol")
        json(syncProtocolFixture());
      else if (url.pathname === "/api/sync/stream") {
        res.writeHead(200, { "Content-Type": "text/event-stream" });
        res.write(apiSchemaHandshakeSse());
        const resources = JSON.parse(url.searchParams.get("resources") || "[]");
        res.write(
          `event: resources\ndata: ${JSON.stringify({
            protocol: 4,
            workspaceId,
            epoch: "fixture-epoch",
            revision: 0,
            reason: "initial",
            resources,
            resourceVersions: resources.map((resource) => ({
              resource,
              revision: 0,
            })),
          })}\n\n`,
        );
        const stream = {
          response: res,
          resources: JSON.parse(url.searchParams.get("resources") || "[]"),
        };
        streams.add(stream);
        res.on("close", () => streams.delete(stream));
      } else if (url.pathname === "/api/sync/pull") {
        const scope = url.searchParams.get("scope");
        const kind = scope.slice(11);
        const count = (attempts.get(kind) || 0) + 1;
        attempts.set(kind, count);
        if (kind === "503" && count === 1)
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
  try {
    const page = fixturePage;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.clock.install();
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(async () => {
      window.client = await import("/src/sync/client.ts");
      const { readRetryDelay } = await import("/src/sync/readRetry.ts");
      window.retryDelay = readRetryDelay(0);
      window.states = [];
      window.stop = window.client.subscribeTranscriptProjection(
        "transcript:503",
        () => {},
        (error) => window.states.push(error ? String(error) : "live"),
      );
    });
    await expect.poll(() => attempts.get("503")).toBe(1);
    const retryDelay = await page.evaluate(() => window.retryDelay);
    await page.clock.runFor(retryDelay);
    await page.waitForFunction(() => window.states.at(-1) === "live");
    assert.equal(
      attempts.get("503"),
      2,
      "A live stream recovers a failed HTTP read",
    );
    await page.clock.runFor(2 * retryDelay);
    assert.equal(attempts.get("503"), 2, "A successful read stops retrying");
    await page.evaluate(() => window.stop());
    for (const kind of ["403", "workspace"]) {
      await page.evaluate((kind) => {
        window.states = [];
        window.stop = window.client.subscribeTranscriptProjection(
          `transcript:${kind}`,
          () => {},
          (error) => window.states.push(error ? String(error) : "live"),
        );
      }, kind);
      await page.waitForFunction(() => window.states.length > 0);
      await page.clock.runFor(2 * retryDelay);
      assert.equal(
        attempts.get(kind),
        1,
        `${kind} must not authorize a retry.`,
      );
      assert.ok(await page.evaluate(() => !window.states.includes("live")));
      await page.evaluate(() => window.stop());
    }
    await page.evaluate(() => {
      window.states = [];
      window.stop = window.client.subscribeTranscriptProjection(
        "transcript:decode",
        () => {},
        (error) => window.states.push(error ? String(error) : "live"),
      );
    });
    await page.waitForFunction(() =>
      window.states.some((state) => state !== "live"),
    );
    invalidate("decode");
    await page.waitForFunction(() => window.states.at(-1) === "live");
    assert.equal(
      attempts.get("decode"),
      2,
      "A new stream hint must recover a malformed projection response.",
    );
    await page.evaluate(() => window.stop());
    expect(errors).toEqual([]);
    console.log("sync read retry browser contract passed");
  } finally {
    for (const stream of streams) stream.response.end();
    await server.close();
  }
});
