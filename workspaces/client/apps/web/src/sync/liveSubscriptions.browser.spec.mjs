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
  test,
} from "../../tests/playwright.ts";

const sse = (name, value) =>
  `event: ${name}\ndata: ${JSON.stringify(value)}\n\n`;

test("switching chats changes subscriptions on one EventSource", async ({
  browser,
}) => {
  const workspaceId = "b".repeat(32);
  const sessions = new Map();
  const updates = [];
  let opened = 0;
  let revision = 0;
  let updateResponseBytes = 0;
  let hypotheticalReopenBytes = 0;
  let hypotheticalReopenRequestBytes = 0;
  const frame = (resources) =>
    sse("resources", {
      protocol: 3,
      workspaceId,
      epoch: "live-subscriptions",
      revision: ++revision,
      reason: "initial",
      resources,
      resourceVersions: resources.map(() => ({ revision })),
    });
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
            const url = new URL(request.url, "http://localhost");
            if (url.pathname === "/live-subscriptions-check") {
              response.setHeader("Content-Type", "text/html");
              response.end(
                '<!doctype html><title>Chat switch check</title><button id="one">Chat one</button><button id="two">Chat two</button><output></output>',
              );
              return;
            }
            if (url.pathname === "/api/sync/stream") {
              const sessionId = `00000000-0000-4000-8000-${String(++opened).padStart(12, "0")}`;
              const resources = JSON.parse(url.searchParams.get("resources"));
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
              const update = JSON.parse(body);
              const session = sessions.get(update.sessionId);
              if (!session) {
                response.writeHead(410);
                response.end();
                return;
              }
              const key = (resource) => JSON.stringify(resource);
              session.resources = session.resources.filter(
                (resource) =>
                  !update.remove.some(
                    (removed) => key(removed) === key(resource),
                  ),
              );
              session.resources.push(...update.add);
              const ack = sse("subscriptions", { gen: update.gen });
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
              response.end(JSON.stringify({ gen: update.gen }));
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
      `http://127.0.0.1:${server.httpServer.address().port}/live-subscriptions-check`,
    );
    await page.evaluate(async (workspaceId) => {
      const { createResourceEvents } =
        await import("/src/sync/resourceEvents.ts");
      const api = await import("/src/api.ts");
      const { openServerEvents } = await import("/src/servers/eventSource.ts");
      api.setToken("fixture-token");
      const transport = createResourceEvents({
        api,
        sync: {
          syncDatabase: async () => ({ workspaceId }),
          persistResourceEntityChanges: async () => {},
        },
        tokenRates: {
          configureTokenRateStream() {},
          receiveResourceTokenRates() {
            return true;
          },
        },
        openEvents: openServerEvents,
        storageName: (name) => name,
      });
      transport.watchResourceChanges({ kind: "state" }, () => {});
      let stop;
      const select = (agentId) => {
        stop?.();
        stop = transport.watchResourceChanges(
          { kind: "transcript", agentId },
          () => {
            document.querySelector("output").textContent = agentId;
          },
        );
      };
      document.querySelector("#one").onclick = () => select("one");
      document.querySelector("#two").onclick = () => select("two");
      window.transport = transport;
      select("one");
    }, workspaceId);
    await expect(page.locator("output")).toHaveText("one");
    await page.locator("#two").click();
    await expect(page.locator("output")).toHaveText("two");
    await page.evaluate(() => {
      for (let i = 0; i < 10; i++) {
        document.querySelector("#one").click();
        document.querySelector("#two").click();
      }
      document.querySelector("#one").click();
    });
    await expect(page.locator("output")).toHaveText("one");
    assert.equal(opened, 1);
    assert.equal(updates.length, 2);
    assert.deepEqual(updates[0].add, [{ kind: "transcript", agentId: "two" }]);
    assert.deepEqual(updates[0].remove, [
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
              Buffer.byteLength(JSON.stringify({ gen: update.gen })),
            0,
          ),
        reopenProtocolBytes:
          hypotheticalReopenBytes + hypotheticalReopenRequestBytes,
      }),
    );
    await page.evaluate(() => window.transport.dispose());
  } finally {
    await context.close();
    for (const { response } of sessions.values()) response.end();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
