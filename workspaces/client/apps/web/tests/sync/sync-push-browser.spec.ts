import type { ServerResponse } from "node:http";
import type { ResourceRef } from "../playwright.ts";
import { fixtureResources } from "./helpers.ts";
import type { IncomingMessage } from "node:http";
import type { Connect } from "vite";
import { fixtureOrigin } from "./helpers.ts";
import { SYNC_PROTOCOL_VERSION } from "../../src/generated/api.ts";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  protocol3SseEvent,
  syncIdentityFixture,
  test,
} from "../playwright.ts";

test("Sync push", async ({ context: testContext }) => {
  test.setTimeout(180_000);
  // Real RxDB/Dexie reconciles a shared stream invalidation through HTTP pull.
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const workspaceId = "c".repeat(32);
  const streams = new Map<ServerResponse, ResourceRef[]>();
  let revision = 1;
  let pulls = 0;
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
      if (url.pathname === "/push-check") res.end("<!doctype html>");
      else if (url.pathname === "/api/sync/identity")
        json(syncIdentityFixture(workspaceId));
      else if (url.pathname === "/api/sync/stream") {
        assert.equal(
          url.searchParams.get("protocol"),
          String(SYNC_PROTOCOL_VERSION),
        );
        const resources = fixtureResources(url.searchParams.get("resources"));
        res.writeHead(200, { "Content-Type": "text/event-stream" });
        res.write(
          apiSchemaHandshakeSse(
            protocol3SseEvent("resources", {
              protocol: 4,
              workspaceId,
              epoch: "fixture-epoch",
              revision,
              reason: "initial",
              resources,
              resourceVersions: resources.map((resource) => ({
                resource,
                revision,
              })),
            }),
          ),
        );
        streams.set(res, resources);
        res.on("close", () => streams.delete(res));
      } else if (url.pathname === "/api/sync/pull") {
        pulls++;
        const row = {
          op: "upsert",
          collection: "agent",
          id: "lead",
          value: {
            id: "lead",
            name: revision === 1 ? "Initial lead" : "Pushed lead",
            status: "running",
          },
          seq: revision,
        };
        json({
          workspaceId,
          documents:
            Number(url.searchParams.get("after")) >= revision ? [] : [row],
          checkpoint: { seq: revision },
          maxSeq: revision,
          initialHigh: revision,
        });
      } else next();
    },
  });
  await server.listen();
  try {
    const page = await testContext.newPage();
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(fixtureOrigin(server) + `/push-check`);
    await page.evaluate(async () => {
      window.syncSyncPushBrowserClient = await import("/src/sync/client.ts");
      window.syncSyncPushBrowserStopPush =
        window.syncSyncPushBrowserClient.subscribeStateProjection(
          (value) => {
            window.syncSyncPushBrowserPushState = value?.threads?.find(
              (row) => row.id === "lead",
            )?.name;
          },
          (error) => {
            if (error) window.syncSyncPushBrowserPushError = String(error);
          },
        );
    });
    await page.waitForFunction(
      () => window.syncSyncPushBrowserPushState === "Initial lead",
    );
    const before = pulls;
    revision = 2;
    for (const [res, resources] of streams)
      res.write(
        protocol3SseEvent("resources", {
          protocol: 4,
          workspaceId,
          epoch: "fixture-epoch",
          revision: 2,
          reason: "change",
          resources,
          resourceVersions: resources.map((resource) => ({
            resource,
            revision: 2,
          })),
        }),
      );
    await page.waitForFunction(
      () => window.syncSyncPushBrowserPushState === "Pushed lead",
    );
    assert.ok(
      pulls > before,
      "The stream does not replace an authoritative pull.",
    );
    assert.equal(streams.size, 1);
    assert.equal(
      await page.evaluate(async () => {
        const { db } = await window.syncSyncPushBrowserClient.syncDatabase();
        const row = await db.projections.findOne("entity:agent:lead").exec();
        if (row?.entity?.collection !== "agent")
          throw new Error("Missing lead entity");
        return row.entity.value.name;
      }),
      "Pushed lead",
    );
    assert.deepEqual(errors, []);
    assert.equal(
      await page.evaluate(() => window.syncSyncPushBrowserPushError),
      undefined,
    );
    await page.evaluate(() => window.syncSyncPushBrowserStopPush());
    console.log("sync push browser contract passed");
  } finally {
    for (const res of streams.keys()) res.end();
    await server.close();
  }
});

declare global {
  interface Window {
    syncSyncPushBrowserClient: typeof import("../../src/sync/client.ts");
    syncSyncPushBrowserStopPush: () => void;
    syncSyncPushBrowserPushState: string | null | undefined;
    syncSyncPushBrowserPushError: string | undefined;
  }
}
