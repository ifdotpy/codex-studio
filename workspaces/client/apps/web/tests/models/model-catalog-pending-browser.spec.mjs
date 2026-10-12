// Actual catalog hook and API error handling. No backend or native model calls.

import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  apiSchemaHandshakeSse,
  apiSchemaHeaders,
  syncIdentityFixture,
  syncProtocolFixture,
  test,
  expect,
} from "../playwright.mjs";

test("Model Catalog Pending Browser", async ({
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

  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const require = createRequire(
    join(root, "workspaces/client/apps/web/package.json"),
  );
  const { createServer } = await import(require.resolve("vite"));
  const temporary = await mkdtemp(
    join(tmpdir(), "studio-model-catalog-pending-"),
  );
  const picker = join(
    root,
    "workspaces/client/apps/web/src/components/agents/WorkerModelPicker.tsx",
  );
  const syncIdentity = syncIdentityFixture();
  let syncStream;
  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: temporary,
    optimizeDeps: {
      include: ["react", "react-dom/client", "react/jsx-dev-runtime"],
    },
    plugins: [
      {
        name: "model-catalog-pending-harness",
        enforce: "pre",
        configureServer(vite) {
          vite.middlewares.use((request, response, next) => {
            const path = request.url?.split("?", 1)[0];
            if (path === "/api/sync/identity") {
              response.writeHead(200, {
                ...apiSchemaHeaders(),
                "Content-Type": "application/json",
              });
              response.end(JSON.stringify(syncIdentity));
              return;
            }
            if (path === "/api/sync/protocol") {
              response.writeHead(200, {
                ...apiSchemaHeaders(),
                "Content-Type": "application/json",
              });
              response.end(JSON.stringify(syncProtocolFixture()));
              return;
            }
            if (path === "/api/test/models-revision") {
              const event = {
                protocol: 3,
                workspaceId: syncIdentity.workspaceId,
                epoch: "fixture",
                revision: 2,
                reason: "models-catalog-completed",
                resources: [{ kind: "models" }],
                resourceVersions: [
                  { resource: { kind: "models" }, revision: 2 },
                ],
              };
              syncStream?.write(
                `event: resources\ndata: ${JSON.stringify(event)}\n\n`,
              );
              response.writeHead(204);
              response.end();
              return;
            }
            if (path === "/api/sync/stream") {
              syncStream = response;
              const resources = JSON.parse(
                new URL(request.url, "http://127.0.0.1").searchParams.get(
                  "resources",
                ) || "[]",
              );
              const event = {
                protocol: 3,
                workspaceId: syncIdentity.workspaceId,
                epoch: "fixture",
                revision: 1,
                reason: "initial",
                resources,
                resourceVersions: resources.map((resource) => ({
                  resource,
                  revision: 1,
                })),
              };
              response.writeHead(200, {
                ...apiSchemaHeaders({ "Content-Type": "text/event-stream" }),
                "Cache-Control": "no-cache",
              });
              response.write(
                apiSchemaHandshakeSse(
                  `event: resources\ndata: ${JSON.stringify(event)}\n\n`,
                ),
              );
              return;
            }
            next();
          });
        },
        resolveId(id) {
          if (id === "virtual:model-catalog-pending") return "\0" + id;
        },
        load(id) {
          if (process.env.BASELINE === "1" && id === picker)
            return execFileSync(
              "git",
              [
                "show",
                "c1e557f:web/src/components/agents/WorkerModelPicker.tsx",
              ],
              { cwd: root, encoding: "utf8" },
            );
          if (id !== "\0virtual:model-catalog-pending") return;
          return `
          import React from "react";
          import {createRoot} from "react-dom/client";
          import {flushSync} from "react-dom";
          import {ModelCatalogReader, useWorkerModels} from "/src/components/agents/WorkerModelPicker.tsx";
          import {get} from "/src/api.ts";
          const reader = {scope: "fixture", read: options => get("/api/models", options)};
          function Fixture({account, workers, enabled}) {
            const result = useWorkerModels(account, enabled, workers);
            window.catalogRenderCount = (window.catalogRenderCount || 0) + 1;
            window.catalogResult = result;
            return React.createElement("pre", null, JSON.stringify(result));
          }
          window.mountCatalog = (account="first", workers=false, enabled=true) => {
            window.catalogRoot ||= createRoot(document.getElementById("app"));
            flushSync(() => window.catalogRoot.render(
              React.createElement(ModelCatalogReader.Provider, {value: reader},
                React.createElement(Fixture, {account, workers, enabled}))));
          };
          window.unmountCatalog = () => {
            flushSync(() => window.catalogRoot?.unmount());
            window.catalogRoot = undefined;
          };
        `;
        },
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    const page = testPage;
    page.setDefaultTimeout(5000);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="app"></div>',
      }),
    );
    await page.goto(server.resolvedUrls.local[0] + "check");
    await page.evaluate(async (schemaHeaders) => {
      window.catalogReplies = [];
      window.catalogRequests = [];
      window.catalogResponses = 0;
      window.catalogResponseRenderCount = 0;
      window.catalogRenderCount = 0;
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (input, options) => {
        const request =
          input instanceof Request ? input : new Request(input, options);
        const requestUrl = request.url;
        if (new URL(requestUrl).pathname !== "/api/models")
          return nativeFetch(input, options);
        window.catalogRequests.push(requestUrl);
        const reply = window.catalogReplies.shift();
        if (!reply) throw new Error("Missing catalog fixture reply");
        if (reply.hold)
          await new Promise((resolve) => {
            window.releaseCatalog = resolve;
          });
        if (reply.network) {
          window.catalogResponses++;
          throw new TypeError("Failed to fetch");
        }
        const response = new Response(JSON.stringify(reply.body), {
          status: reply.status || 200,
          headers: {
            "Content-Type": "application/json",
            ...schemaHeaders,
          },
        });
        window.catalogResponseRenderCount = window.catalogRenderCount;
        window.catalogResponses++;
        return response;
      };
      await import("/@id/__x00__virtual:model-catalog-pending");
    }, apiSchemaHeaders());
    const model = (value) => ({ model: value, displayName: value });
    const ready = (value) => ({ body: { data: [model(value)] } });
    const pending = {
      status: 400,
      body: { error: "Model catalog is pending", catalogPending: true },
    };
    const settleCatalogRead = async () => {
      await page.waitForFunction(
        () => window.catalogRequests.length === window.catalogResponses,
      );
      await page.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
    };
    const reset = async (replies, account = "first", workers = false) => {
      await settleCatalogRead();
      await page.evaluate(
        ({ replies, account, workers }) => {
          window.unmountCatalog();
          window.catalogReplies = replies;
          window.catalogRequests = [];
          window.catalogResponses = 0;
          window.catalogResponseRenderCount = window.catalogRenderCount;
          window.mountCatalog(account, workers);
        },
        { replies, account, workers },
      );
      await page.waitForFunction(() => window.catalogRequests.length > 0);
      if (!replies[0]?.hold)
        await page.waitForFunction(() => window.catalogResponses > 0);
    };
    const retry = async () => {
      const before = await page.evaluate(() => window.catalogRequests.length);
      const responsesBefore = await page.evaluate(
        () => window.catalogResponses,
      );
      await page.evaluate(() => window.catalogResult.retry());
      await page.waitForFunction(
        (before) => window.catalogRequests.length > before,
        before,
      );
      await page.waitForFunction(
        (before) => window.catalogResponses > before,
        responsesBefore,
      );
    };
    const hasModel = (expected) =>
      page.waitForFunction(
        (expected) =>
          window.catalogResult.models.some((row) => row.model === expected),
        expected,
      );

    await reset([pending, ready("late-model")]);
    await page.waitForFunction(
      () => window.catalogRenderCount > window.catalogResponseRenderCount,
    );
    assert.equal(await page.evaluate(() => window.catalogResult.loading), true);
    assert.equal(await page.evaluate(() => window.catalogResult.error), "");
    assert.equal(await page.evaluate(() => window.catalogRequests.length), 1);
    await retry();
    await hasModel("late-model");
    assert.match(
      await page.evaluate(() => window.catalogRequests.at(-1)),
      /[?&]retry=1(?:&|$)/,
    );

    await page.evaluate(() => {
      window.catalogReplies = [
        {
          status: 400,
          body: { error: "Model catalog is pending", catalogPending: true },
        },
        { body: { data: [{ model: "updated-model" }] } },
      ];
      window.catalogResult.retry();
    });
    await page.waitForFunction(
      () => window.catalogRenderCount > window.catalogResponseRenderCount,
    );
    await hasModel("late-model");
    assert.equal(
      await page.evaluate(() => window.catalogResult.loading),
      false,
    );
    assert.equal(await page.evaluate(() => window.catalogRequests.length), 3);
    await retry();
    await hasModel("updated-model");
    assert.deepEqual(
      await page.evaluate(() =>
        window.catalogResult.models.map((row) => row.model),
      ),
      ["updated-model"],
    );

    await reset([ready("old-worker-model")], "first", true);
    await hasModel("old-worker-model");
    await page.evaluate(() => {
      window.catalogReplies = [
        {
          body: {
            data: [{ model: "partial-worker-model" }],
            catalogPending: true,
            unavailableAccounts: [
              { accountKey: "second", error: "Pending", catalogPending: true },
            ],
          },
        },
        {
          body: {
            data: [{ model: "complete-worker-model" }],
            unavailableAccounts: [],
          },
        },
      ];
      window.catalogResult.retry();
    });
    await hasModel("partial-worker-model");
    await hasModel("old-worker-model");
    await page.waitForFunction(
      () => window.catalogRenderCount > window.catalogResponseRenderCount,
    );
    await retry();
    await hasModel("complete-worker-model");
    assert.deepEqual(
      await page.evaluate(() =>
        window.catalogResult.models.map((row) => row.model),
      ),
      ["complete-worker-model"],
    );

    const permanentReplies = [
      { status: 400, body: { error: "Model catalog is pending" } },
      { status: 400, body: { error: "Pending", catalogPending: "true" } },
      { status: 503, body: { error: "Pending", catalogPending: true } },
      { network: true },
    ];
    for (const reply of permanentReplies) {
      await page.evaluate((reply) => {
        window.catalogReplies = [reply];
        window.catalogResult.retry();
      }, reply);
      await page.waitForFunction(() => !!window.catalogResult.error);
      await hasModel("complete-worker-model");
    }
    for (const [index, unavailableAccounts] of [
      undefined,
      [],
      [{ accountKey: "second", error: "Pending", catalogPending: "true" }],
      [{ accountKey: "second", catalogPending: true }],
    ].entries()) {
      await reset(
        [
          {
            body: {
              data: [model("valid-model")],
              catalogPending: true,
              unavailableAccounts,
            },
          },
        ],
        `valid-${index}`,
      );
      await hasModel("valid-model");
    }

    await reset([pending], "pending-remount");
    await page.waitForFunction(
      () => window.catalogRenderCount > window.catalogResponseRenderCount,
    );
    await page.evaluate((reply) => {
      window.unmountCatalog();
      window.catalogReplies = [reply];
      window.catalogRequests = [];
      window.catalogResponses = 0;
      window.mountCatalog("pending-remount");
    }, ready("completed-after-revision"));
    await page.evaluate(
      () =>
        new Promise((resolve) =>
          requestAnimationFrame(() => requestAnimationFrame(resolve)),
        ),
    );
    assert.equal(await page.evaluate(() => window.catalogRequests.length), 0);
    await page.evaluate(() => fetch("/api/test/models-revision"));
    await page.waitForFunction(() => window.catalogRequests.length === 1);
    await page.waitForFunction(() => window.catalogResponses === 1);
    await hasModel("completed-after-revision");
    assert.equal(await page.evaluate(() => window.catalogRequests.length), 1);
    await page.evaluate(() =>
      window.mountCatalog("pending-remount", false, false),
    );
    assert.equal(await page.evaluate(() => window.catalogRequests.length), 1);

    await reset([{ ...ready("old-account-model"), hold: true }], "hold-abort");
    await page.evaluate(() => {
      window.oldCatalogRelease = window.releaseCatalog;
      window.catalogReplies = [
        { body: { data: [{ model: "new-account-model" }] } },
      ];
      window.mountCatalog("second");
    });
    await hasModel("new-account-model");
    await page.evaluate(() => window.oldCatalogRelease());
    assert.deepEqual(
      await page.evaluate(() =>
        window.catalogResult.models.map((row) => row.model),
      ),
      ["new-account-model"],
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS model catalog pending: cold and partial retry, old models, permanent errors, scope isolation",
    );
  } finally {
    syncStream?.end();
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
