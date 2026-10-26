import type { components } from "../generated/api";
import type { ResourceRef } from "./resourceEvents";
import { initializeSubscriptions } from "./liveSubscriptions.browser.fixture.ts";
import {
  fixturePort,
  type LegacySubscriptionFrame,
  fixtureSubscriptions,
  fixtureResources,
} from "./syncBrowserFixture.ts";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  apiSchemaHandshakeSse,
  createFixtureViteServer,
  expect,
  readApiSchemaHash,
  protocol3SseEvent as sse,
  test,
} from "../../tests/playwright.ts";

test("switching chats changes subscriptions on one EventSource", async ({
  browser,
}) => {
  const workspaceId = "b".repeat(32);
  const sessions = new Map<
    string,
    {
      response: import("node:http").ServerResponse;
      resources: (ResourceRef | components["schemas"]["SubscriptionAdd"])[];
      url: URL;
    }
  >();
  const updates: (components["schemas"]["SyncSubscriptionRequest"] & {
    bytes: number;
  })[] = [];
  let opened = 0;
  let revision = 0;
  let updateResponseBytes = 0;
  let hypotheticalReopenBytes = 0;
  let hypotheticalReopenRequestBytes = 0;
  const frame = (
    resources: (ResourceRef | components["schemas"]["SubscriptionAdd"])[],
  ) =>
    sse("resources", {
      protocol: 3,
      workspaceId,
      epoch: "live-subscriptions",
      revision: ++revision,
      reason: "initial",
      resources,
      resourceVersions: resources.map(() => ({ revision })),
    } satisfies LegacySubscriptionFrame);
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-live-subscriptions-"));
  const server = await createFixtureViteServer({
    configFile: false,
    cacheDir,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "live-subscriptions-fixture",
        configureServer(server) {
          server.middlewares.use(async (request, response, next) => {
            const url = new URL(request.url ?? "", "http://localhost");
            if (url.pathname === "/live-subscriptions-check") {
              response.setHeader("Content-Type", "text/html");
              response.end(
                '<!doctype html><title>Chat switch check</title><button id="one">Chat one</button><button id="two">Chat two</button><output></output>',
              );
              return;
            }
            if (url.pathname === "/api/sync/stream") {
              const sessionId = `00000000-0000-4000-8000-${String(++opened).padStart(12, "0")}`;
              const resources = fixtureResources(
                url.searchParams.get("resources"),
              );
              sessions.set(sessionId, { response, resources, url });
              response.on("close", () => sessions.delete(sessionId));
              response.setHeader("Content-Type", "text/event-stream");
              response.write(sse("session", { sessionId }));
              response.write(apiSchemaHandshakeSse());
              response.write(frame(resources));
              return;
            }
            if (url.pathname === "/api/sync/subscriptions") {
              const chunks = [];
              for await (const chunk of request) chunks.push(chunk);
              const body = Buffer.concat(chunks);
              const update = fixtureSubscriptions(body.toString());
              const session = sessions.get(update.sessionId);
              if (!session) {
                response.writeHead(410);
                response.end();
                return;
              }
              const key = (
                resource:
                  | ResourceRef
                  | components["schemas"]["SubscriptionAdd"],
              ) => JSON.stringify(resource);
              session.resources = session.resources.filter(
                (resource) =>
                  !update.remove.some(
                    (removed) => key(removed) === key(resource),
                  ),
              );
              session.resources.push(...update.add);
              const ack = sse("subscriptions", {
                gen: update.gen,
              } satisfies components["schemas"]["SyncSubscriptionAck"]);
              const initial = frame(update.add);
              updates.push({ ...update, bytes: body.length });
              updateResponseBytes += Buffer.byteLength(ack + initial);
              hypotheticalReopenBytes += Buffer.byteLength(
                sse("session", { sessionId: update.sessionId }) +
                  apiSchemaHandshakeSse() +
                  frame(session.resources),
              );
              const reopen = new URL(session.url);
              reopen.searchParams.set(
                "resources",
                JSON.stringify(session.resources),
              );
              reopen.searchParams.set("tokenRates", String(update.tokenRates));
              hypotheticalReopenRequestBytes += Buffer.byteLength(
                reopen.pathname + reopen.search,
              );
              session.response.write(ack + initial);
              response.setHeader("Content-Type", "application/json");
              response.setHeader(API_SCHEMA_HASH_HEADER, readApiSchemaHash());
              response.end(
                JSON.stringify({
                  gen: update.gen,
                } satisfies components["schemas"]["SyncSubscriptionAck"]),
              );
              return;
            }
            next();
          });
        },
      },
    ],
  });
  const context = await browser.newContext();
  try {
    await server.listen();
    const page = await context.newPage();
    await page.goto(
      `http://127.0.0.1:${fixturePort(server)}/live-subscriptions-check`,
    );
    await page.evaluate(initializeSubscriptions, workspaceId);
    await expect(page.locator("output")).toHaveText("one");
    await page.locator("#two").click();
    await expect(page.locator("output")).toHaveText("two");
    await page.evaluate(() => {
      const one = document.querySelector<HTMLButtonElement>("#one");
      const two = document.querySelector<HTMLButtonElement>("#two");
      if (!one || !two) throw new Error("Missing subscription buttons");
      for (let i = 0; i < 10; i++) {
        one.click();
        two.click();
      }
      one.click();
    });
    await expect(page.locator("output")).toHaveText("one");
    assert.equal(opened, 1);
    assert.equal(updates.length, 2);
    assert.deepEqual(updates[0]?.add, [{ kind: "transcript", agentId: "two" }]);
    assert.deepEqual(updates[0]?.remove, [
      { kind: "transcript", agentId: "one" },
    ]);
    console.log(
      JSON.stringify({
        opened,
        chatSelections: 22,
        posts: updates.length,
        requestBodyBytes: updates.reduce(
          (total, update) => total + update.bytes,
          0,
        ),
        streamUpdateBytes: updateResponseBytes,
        hypotheticalReopenStreamBytes: hypotheticalReopenBytes,
        liveProtocolBytes:
          updateResponseBytes +
          updates.reduce(
            (total, update) =>
              total +
              update.bytes +
              Buffer.byteLength("/api/sync/subscriptions") +
              Buffer.byteLength(
                JSON.stringify({
                  gen: update.gen,
                } satisfies components["schemas"]["SyncSubscriptionAck"]),
              ),
            0,
          ),
        reopenProtocolBytes:
          hypotheticalReopenBytes + hypotheticalReopenRequestBytes,
      }),
    );
    await page.evaluate(() => window.syncSubscriptions.transport.dispose());
  } finally {
    await context.close();
    for (const { response } of sessions.values()) response.end();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
