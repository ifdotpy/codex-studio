import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

import { test, expect, apiSchemaHandshakeSse } from "../playwright.mjs";

test("sync-read-retry-browser", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
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
          protocol: 3,
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
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
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
        res.write(apiSchemaHandshakeSse());
        const resources = JSON.parse(url.searchParams.get("resources") || "[]");
        res.write(
          `event: resources\ndata: ${JSON.stringify({
            protocol: 3,
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
        if (["429", "503"].includes(kind) && count === 1)
          return json({ error: "Temporary failure" }, Number(kind));
        if (kind === "persistent503")
          return json({ error: "Temporary failure" }, 503);
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
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.evaluate(async () => {
      window.client = await import("/src/sync/client.ts");
      const { NetworkTimeoutError, ApiError } = await import("/src/api.ts");
      const originalFetch = window.fetch;
      window.transientAttempts = {};
      window.fetch = (...args) => {
        const request =
          args[0] instanceof Request ? args[0] : new Request(args[0], args[1]);
        const url = new URL(request.url);
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
        window.stop = window.client.subscribeTranscriptProjection(
          `transcript:${kind}`,
          () => {},
          (error) => window.states.push(error ? String(error) : "live"),
        );
      }, kind);
      try {
        for (
          let attempt = 0;
          attempt < 100 && attempts.get(kind) !== 2;
          attempt++
        )
          await new Promise((resolve) => setTimeout(resolve, 20));
        await new Promise((resolve) => setTimeout(resolve, 500));
        assert.equal(
          attempts.get(kind),
          ["timeout", "network", "408"].includes(kind) ? 1 : 2,
          `${kind} retries once on the same live-stream notification`,
        );
        if (["timeout", "network", "408"].includes(kind))
          assert.equal(
            await page.evaluate((kind) => window.transientAttempts[kind], kind),
            2,
          );
        await new Promise((resolve) => setTimeout(resolve, 500));
        assert.equal(
          attempts.get(kind),
          ["timeout", "network", "408"].includes(kind) ? 1 : 2,
          `${kind} does not continue retrying during a healthy quiet period`,
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
      await page.evaluate(() => window.stop());
    }
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
      await new Promise((resolve) => setTimeout(resolve, 600));
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
        "transcript:persistent503",
        () => {},
        (error) => window.states.push(error ? String(error) : "live"),
      );
    });
    for (
      let attempt = 0;
      attempt < 100 && attempts.get("persistent503") !== 3;
      attempt++
    )
      await new Promise((resolve) => setTimeout(resolve, 20));
    assert.equal(attempts.get("persistent503"), 3);
    assert.ok(
      await page.evaluate(() =>
        window.states.some((state) => state !== "live"),
      ),
      "The exhausted transient retry budget leaves a visible read error",
    );
    await new Promise((resolve) => setTimeout(resolve, 800));
    assert.equal(
      attempts.get("persistent503"),
      3,
      "An exhausted notification does not trigger perpetual retries",
    );
    await page.evaluate(() => window.stop());
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
