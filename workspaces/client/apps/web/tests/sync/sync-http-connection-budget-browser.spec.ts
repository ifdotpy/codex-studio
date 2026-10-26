import type { FixtureResourceChange } from "./helpers.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import type { ResourceRef } from "../playwright.ts";
import { fixtureResources, fixturePostBody } from "./helpers.ts";
import type { IncomingMessage, ServerResponse } from "node:http";
import type { Connect } from "vite";
import { fixtureOrigin } from "./helpers.ts";
import type {} from "./sync-http-connection-budget-browser.fixture.tsx";
import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
} from "../playwright.ts";
// Actual HTTP connection capacity with two renderer windows and slow history.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("Sync http connection budget browser", async ({
  browser: testBrowser,
  context: runnerContext,
}) => {
  test.setTimeout(180_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const cache = await mkdtemp(join(tmpdir(), "studio-sync-connection-budget-"));
  const workspaceId = "b".repeat(32);
  const streams = new Set<ServerResponse>();
  const streamResources = new Map<ServerResponse, ResourceRef[]>();
  const held = new Set<ServerResponse>();
  const requests: {
    path: string;
    query: string;
    at: number;
    method: string | undefined;
  }[] = [];
  const messages = new Map<
    string,
    { body: PostBody<"/api/messages">; requests: number }
  >();
  let revision = 1;
  const changeState = () => {
    for (const response of streams) {
      const refs = streamResources.get(response) || [];
      if (!refs.some((resource) => resource.kind === "state")) continue;
      response.write(
        `event: resources\ndata: ${JSON.stringify({
          protocol: 4,
          workspaceId,
          epoch: "budget-epoch",
          revision,
          reason: "change",
          resources: [{ kind: "state" }],
          resourceVersions: [{ resource: { kind: "state" }, revision }],
        } satisfies FixtureResourceChange)}\n\n`,
      );
    }
  };
  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: cache,
    optimizeDeps: {
      include: [
        "react",
        "react-dom/client",
        "react/jsx-dev-runtime",
        "rxdb",
        "rxdb/plugins/storage-dexie",
        "rxdb/plugins/leader-election",
        "rxdb/plugins/replication",
      ],
    },
    plugins: [
      {
        name: "connection-budget-harness",
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
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
        res.writeHead(
          200,
          apiSchemaHeaders({ "Content-Type": "application/json" }),
        );
        res.end(JSON.stringify(value));
      };
      if (url.pathname === "/check") {
        res.setHeader("Content-Type", "text/html");
        res.end(
          '<!doctype html><div id="app"></div><script type="module" src="/tests/sync/sync-http-connection-budget-browser.fixture.tsx"></script>',
        );
      } else if (url.pathname.startsWith("/api/")) {
        requests.push({
          path: url.pathname,
          query: url.search,
          at: Date.now(),
          method: req.method,
        });
        if (url.pathname === "/api/session") json({ token: "fixture-token" });
        else if (url.pathname === "/api/sync/identity")
          json(syncIdentityFixture(workspaceId));
        else if (url.pathname === "/api/sync/protocol")
          json(syncProtocolFixture());
        else if (url.pathname === "/api/sync/stream") {
          res.writeHead(200, {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache",
          });
          res.write(apiSchemaHandshakeSse());
          res.write(": connected\n\n");
          streams.add(res);
          const refs = fixtureResources(url.searchParams.get("resources"));
          streamResources.set(res, refs);
          res.write(
            `event: resources\ndata: ${JSON.stringify({
              protocol: 4,
              workspaceId,
              epoch: "budget-epoch",
              revision,
              reason: "initial",
              resources: refs,
              resourceVersions: refs.map((resource) => ({
                resource,
                revision,
              })),
            } satisfies FixtureResourceChange)}\n\n`,
          );
          res.on("close", () => {
            streams.delete(res);
            streamResources.delete(res);
          });
        } else if (url.pathname === "/api/sync/pull") {
          const scope = url.searchParams.get("scope") ?? "";
          if (scope.startsWith("transcript:warm-")) {
            held.add(res);
            res.on("close", () => held.delete(res));
            return;
          }
          const document =
            scope === "state:entities:v1"
              ? {
                  op: "upsert",
                  collection: "workspace",
                  id: "current",
                  value: {
                    stateDir: "/fixture",
                    marker: "revision-" + revision,
                  },
                  seq: revision,
                }
              : scope.startsWith("transcript:")
                ? { id: scope, seq: 1, payload: JSON.stringify({ items: [] }) }
                : null;
          json({
            workspaceId,
            documents:
              Number(url.searchParams.get("after")) >= revision
                ? []
                : document
                  ? [document]
                  : [],
            checkpoint: { seq: revision },
            maxSeq: revision,
            initialHigh: revision,
          });
        } else if (url.pathname === "/api/messages") {
          let body = "";
          req.on("data", (value) => {
            body += value;
          });
          req.on("end", () => {
            const message = fixturePostBody<"/api/messages">(JSON.parse(body));
            assert.ok(message.id);
            const previous = messages.get(message.id);
            if (previous) assert.deepEqual(previous.body, message);
            messages.set(message.id, {
              body: message,
              requests: (previous?.requests || 0) + 1,
            });
            json({ id: message.id, status: "accepted" });
          });
        } else json({ documents: [], items: [] });
      } else next();
    },
  });
  try {
    await server.listen();
    const context = runnerContext;
    const pages = [await context.newPage(), await context.newPage()] as const;
    const errors: string[] = [];
    for (const page of pages)
      page.on("pageerror", (error) => errors.push(error.message));
    const origin = fixtureOrigin(server);
    await Promise.all(pages.map((page) => page.goto(origin + "/check")));
    await Promise.all(
      pages.map((page) =>
        page
          .waitForFunction(
            () => window.syncHttpConnectionBudgetBrowserFixtureMount,
            undefined,
            { timeout: 15_000 },
          )
          .catch(async (error) => {
            throw new Error(
              `${error.message}; page errors: ${JSON.stringify(errors)}`,
            );
          }),
      ),
    );
    await Promise.all(
      pages.map((page, index) =>
        page.evaluate(
          (index) =>
            window.syncHttpConnectionBudgetBrowserFixtureMount(
              "selected-" + index,
            ),
          index,
        ),
      ),
    );
    await Promise.all(
      pages.map((page) =>
        page.waitForFunction(
          () => window.syncHttpConnectionBudgetBrowserFixtureSnapshot?.data,
        ),
      ),
    );
    await new Promise((resolve) => setTimeout(resolve, 400));
    const sessionCount = requests.filter(
      (request) => request.path === "/api/session",
    ).length;
    const started = Date.now();
    await pages[0].evaluate(() => {
      window.syncHttpConnectionBudgetBrowserFixtureRefresh =
        window.syncHttpConnectionBudgetBrowserFixtureSnapshot.refresh();
    });
    await pages[0].waitForFunction(
      () =>
        window.syncHttpConnectionBudgetBrowserFixtureSnapshot.error ||
        window.syncHttpConnectionBudgetBrowserFixtureSnapshot.data?.token,
      undefined,
      { timeout: 20000 },
    );
    await Promise.race([
      pages[0].evaluate(
        () => window.syncHttpConnectionBudgetBrowserFixtureRefresh,
      ),
      new Promise((_, reject) =>
        setTimeout(() => reject(new Error("refresh did not terminate")), 18000),
      ),
    ]);
    const elapsedMs = Date.now() - started;
    const banner = await pages[0].locator("#error").textContent();
    console.log(
      JSON.stringify({
        streams: streams.size,
        elapsedMs,
        banner,
        sessionReachedServer:
          requests.filter((request) => request.path === "/api/session").length >
          sessionCount,
      }),
    );
    assert.equal(
      banner,
      "",
      "healthy session endpoint must remain reachable with two renderer windows",
    );
    assert.ok(
      elapsedMs < 1000,
      "a control GET must not wait for stream connections",
    );
    assert.ok(
      streams.size <= 1,
      "renderer windows must share one workspace stream",
    );

    const pullsBefore = requests.filter(
      (request) => request.path === "/api/sync/pull",
    ).length;
    revision++;
    changeState();
    await Promise.all(
      pages.map((page) =>
        page.waitForFunction(() => {
          const runtime =
            window.syncHttpConnectionBudgetBrowserFixtureSnapshot.data?.runtime;
          return (
            runtime && "marker" in runtime && runtime.marker === "revision-2"
          );
        }),
      ),
    );
    assert.ok(
      requests.filter((request) => request.path === "/api/sync/pull").length >
        pullsBefore,
      "a shared invalidation must reconcile through the authoritative pull",
    );

    // Slow background history must retain capacity for reads and durable writes.
    await Promise.all(
      pages.map((page, index) =>
        page.evaluate(
          ({ workspaceId, index }) => {
            window.syncHttpConnectionBudgetBrowserFixtureWarm = window
              .syncHttpConnectionBudgetBrowserFixturePrefetchTranscript(
                workspaceId,
                "warm-" + index,
              )
              .catch(() => false);
          },
          { workspaceId, index },
        ),
      ),
    );
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.equal(held.size, 2);
    await Promise.all(
      pages.map((page) =>
        page.evaluate(async (workspaceId) => {
          if (
            await window.syncHttpConnectionBudgetBrowserFixturePrefetchTranscript(
              workspaceId,
              "warm-over-limit",
            )
          )
            throw new Error(
              "a second background history request exceeded the limit",
            );
        }, workspaceId),
      ),
    );
    const controlStarted = Date.now();
    await Promise.all(
      pages.map((page, index) =>
        page.evaluate(async (index) => {
          await window.syncHttpConnectionBudgetBrowserFixtureSnapshot.refresh();
          const id = "exact-request-" + index;
          const receipt =
            await window.syncHttpConnectionBudgetBrowserFixtureDurableSend({
              id,
              room: "selected-" + index,
              text: "fixture",
            });
          if (receipt.id !== id) throw new Error("request identity changed");
        }, index),
      ),
    );
    assert.ok(Date.now() - controlStarted < 1000);
    assert.equal(messages.size, 2);
    assert.ok([...messages.values()].every((value) => value.requests === 1));
    assert.deepEqual(errors, []);
    console.log("sync HTTP connection budget browser contract passed");
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
    for (const response of streams) response.end();
    for (const response of held) response.end();
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
