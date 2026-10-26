import type { ServerResponse } from "node:http";
import type { IncomingMessage } from "node:http";
import type { Connect } from "vite";
import { fixtureOrigin, fixtureAssert as assert } from "./helpers.ts";
import { fileURLToPath } from "node:url";
import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
  expect,
} from "../playwright.ts";

test("Sync Push Lifecycle Browser", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const workspaceId = "d".repeat(32);
  const streams = new Set<ServerResponse>();
  let opened = 0;
  let identityResponse: (() => void) | undefined;
  let delayIdentity = true;
  const server = await createServer({
    configFile: false,
    optimizeDeps: { noDiscovery: true },
    root: fileURLToPath(new URL("../../", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
  });
  server.middlewares.stack.unshift({
    route: "",
    handle(
      req: IncomingMessage,
      res: ServerResponse,
      next: Connect.NextFunction,
    ) {
      const url = new URL(req.url ?? "/", "http://localhost");
      const json = (value: unknown) => {
        for (const [name, value] of Object.entries(
          apiSchemaHeaders({ "Content-Type": "application/json" }),
        ))
          res.setHeader(name, value);
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") res.end("<!doctype html>");
      else if (url.pathname === "/api/sync/protocol")
        json(syncProtocolFixture());
      else if (url.pathname === "/api/sync/identity") {
        if (delayIdentity)
          identityResponse = () => json(syncIdentityFixture(workspaceId));
        else json(syncIdentityFixture(workspaceId));
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
  const origin = fixtureOrigin(server);
  try {
    const page = testPage;
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.clock.install();
    await page.goto(`${origin}/check`);
    await page.evaluate(async () => {
      window.syncSyncPushLifecycleBrowserClient =
        await import("/src/sync/client.ts");
      window.syncSyncPushLifecycleBrowserStopPush =
        window.syncSyncPushLifecycleBrowserClient.watchSyncInvalidations(
          "entities",
          () => {},
        );
    });
    await expect.poll(() => Boolean(identityResponse)).toBe(true);
    await page.evaluate(() => window.syncSyncPushLifecycleBrowserStopPush());
    delayIdentity = false;
    if (!identityResponse) throw new Error("Missing delayed identity response");
    identityResponse();
    await page.evaluate(() =>
      window.syncSyncPushLifecycleBrowserClient.syncDatabase(),
    );
    await page.clock.runFor(250);
    assert.equal(
      opened,
      0,
      "A disposed subscription must not open a delayed stream.",
    );

    await page.evaluate(() => {
      window.syncSyncPushLifecycleBrowserStopPush =
        window.syncSyncPushLifecycleBrowserClient.watchSyncInvalidations(
          "entities",
          () => {},
        );
    });
    await expect.poll(() => streams.size).toBe(1);
    for (const response of streams) response.end();
    await expect.poll(() => streams.size).toBe(0);
    await page.evaluate(() => window.syncSyncPushLifecycleBrowserStopPush());
    const beforeRetry = opened;
    await page.clock.runFor(1200);
    assert.equal(
      opened,
      beforeRetry,
      "A disposed subscription must cancel reconnect attempts.",
    );

    await page.evaluate(() => {
      window.syncSyncPushLifecycleBrowserStops = [
        window.syncSyncPushLifecycleBrowserClient.watchSyncInvalidations(
          "entities",
          () => {},
        ),
        window.syncSyncPushLifecycleBrowserClient.watchSyncInvalidations(
          "entities",
          () => {},
        ),
      ];
    });
    await expect.poll(() => streams.size).toBeGreaterThan(0);
    await page.clock.runFor(250);
    assert.equal(
      streams.size,
      1,
      "Subscriptions to one scope must share one stream.",
    );
    await page.evaluate(() => window.syncSyncPushLifecycleBrowserStops[0]());
    assert.equal(streams.size, 1, "The remaining subscriber keeps its stream.");
    await page.evaluate(() => window.syncSyncPushLifecycleBrowserStops[1]());
    await expect.poll(() => streams.size).toBe(0);
    assert.deepEqual(errors, []);
    console.log("sync push lifecycle browser contract passed");
  } finally {
    for (const response of streams) response.end();
    await server.close();
  }
});

declare global {
  interface Window {
    syncSyncPushLifecycleBrowserClient: typeof import("../../src/sync/client.ts");
    syncSyncPushLifecycleBrowserStopPush: () => void;
    syncSyncPushLifecycleBrowserStops: [() => void, () => void];
  }
}
