import { fileURLToPath } from "node:url";
import { apiSchemaHandshakeSse, test, expect } from "../playwright.mjs";

test("Sync Push Lifecycle Browser", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const assert = {
    equal: (actual, expected, message) =>
      expect(actual, message).toBe(expected),
    notEqual: (actual, expected, message) =>
      expect(actual, message).not.toBe(expected),
    deepEqual: (actual, expected, message) =>
      expect(actual, message).toEqual(expected),
    ok: (actual, message) => expect(actual, message).toBeTruthy(),
    match: (actual, expected, message) =>
      expect(actual, message).toMatch(expected),
    doesNotMatch: (actual, expected, message) =>
      expect(actual, message).not.toMatch(expected),
    fail: (message) => {
      throw new Error(message);
    },
  };

  const { createServer } = await import(
    new URL(
      "../../../web/node_modules/vite/dist/node/index.js",
      import.meta.url,
    )
  );
  const workspaceId = "d".repeat(32);
  const streams = new Set();
  let opened = 0;
  let identityResponse;
  let delayIdentity = true;
  const server = await createServer({
    configFile: false,
    root: fileURLToPath(new URL("../../../web", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.stack.unshift({
    route: "",
    handle(req, res, next) {
      const url = new URL(req.url, "http://localhost");
      const json = (value) => {
        res.setHeader("Content-Type", "application/json");
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") res.end("<!doctype html>");
      else if (url.pathname === "/api/sync/protocol")
        json({ protocolVersion: 1, capabilities: ["streamChanges"] });
      else if (url.pathname === "/api/sync/identity") {
        if (delayIdentity) identityResponse = () => json({ workspaceId });
        else json({ workspaceId });
      } else if (url.pathname === "/api/sync/stream") {
        opened++;
        res.writeHead(200, { "Content-Type": "text/event-stream" });
        res.write(apiSchemaHandshakeSse());
        streams.add(res);
        res.on("close", () => streams.delete(res));
      } else next();
    },
  });
  await server.listen();
  const origin = `http://127.0.0.1:${server.httpServer.address().port}`;
  const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  async function waitFor(predicate) {
    for (let attempt = 0; attempt < 100; attempt++) {
      if (predicate()) return;
      await pause(20);
    }
    assert.fail("The stream lifecycle condition did not complete.");
  }
  try {
    const page = testPage;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(`${origin}/check`);
    await page.evaluate(async () => {
      window.client = await import("/src/sync/client.ts");
      window.stopPush = window.client.watchSyncInvalidations(
        "entities",
        () => {},
      );
    });
    await waitFor(() => identityResponse);
    await page.evaluate(() => window.stopPush());
    delayIdentity = false;
    identityResponse();
    await page.evaluate(() => window.client.syncDatabase());
    await pause(250);
    assert.equal(
      opened,
      0,
      "A disposed subscription must not open a delayed stream.",
    );

    await page.evaluate(() => {
      window.stopPush = window.client.watchSyncInvalidations(
        "entities",
        () => {},
      );
    });
    await waitFor(() => streams.size === 1);
    for (const response of streams) response.end();
    await waitFor(() => streams.size === 0);
    await page.evaluate(() => window.stopPush());
    const beforeRetry = opened;
    await pause(1200);
    assert.equal(
      opened,
      beforeRetry,
      "A disposed subscription must cancel reconnect attempts.",
    );

    await page.evaluate(() => {
      window.stops = [
        window.client.watchSyncInvalidations("entities", () => {}),
        window.client.watchSyncInvalidations("entities", () => {}),
      ];
    });
    await waitFor(() => streams.size > 0);
    await pause(250);
    assert.equal(
      streams.size,
      1,
      "Subscriptions to one scope must share one stream.",
    );
    await page.evaluate(() => window.stops[0]());
    assert.equal(streams.size, 1, "The remaining subscriber keeps its stream.");
    await page.evaluate(() => window.stops[1]());
    await waitFor(() => streams.size === 0);
    assert.deepEqual(errors, []);
    console.log("sync push lifecycle browser contract passed");
  } finally {
    for (const response of streams) response.end();
    await server.close();
  }
});
