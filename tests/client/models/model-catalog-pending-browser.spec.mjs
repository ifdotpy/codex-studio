// Actual catalog hook and API error handling. No backend or native model calls.

import { FIXTURE_OPTIMIZE_DEPS } from "../playwright.mjs";
import { execFileSync } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  handleEntitySyncFixtureRequest,
  readApiSchemaHash,
  syncIdentityFixture,
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

  const root = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );

  const { createFixtureViteServer: createServer } =
    await import("../playwright.mjs");
  const temporary = await mkdtemp(
    join(tmpdir(), "studio-model-catalog-pending-"),
  );
  const picker = join(root, "web/src/components/agents/WorkerModelPicker.tsx");
  const identity = syncIdentityFixture("0123456789abcdef0123456789abcdef");
  const resourceNotifiers = new Set();
  let resourceNotifySequence = 0;
  const syncFixture = {
    snapshot: {},
    workspaceId: identity.workspaceId,
    onStreamReady: (notify) => resourceNotifiers.add(notify),
  };
  const server = await createServer({
    configFile: false,
    root: join(root, "web"),
    cacheDir: temporary,
    optimizeDeps: {
      noDiscovery: true,
      include: [
        ...FIXTURE_OPTIMIZE_DEPS.include,
        "react",
        "react-dom/client",
        "react/jsx-dev-runtime",
      ],
    },
    plugins: [
      {
        name: "model-catalog-pending-harness",
        enforce: "pre",
        configureServer(server) {
          server.middlewares.stack.unshift({
            route: "",
            handle(request, response, next) {
              const url = new URL(request.url || "/", "http://localhost");
              if (url.pathname === "/api/sync/notify") {
                const chunks = [];
                request.on("data", (chunk) => chunks.push(chunk));
                request.on("end", () => {
                  const body = JSON.parse(Buffer.concat(chunks).toString());
                  for (const notify of resourceNotifiers)
                    notify(body.resources);
                  response.setHeader("Content-Type", "application/json");
                  response.end(
                    JSON.stringify({
                      accepted: true,
                      requestId: body.requestId,
                    }),
                  );
                });
                return;
              }
              if (
                handleEntitySyncFixtureRequest(request, response, syncFixture)
              )
                return;
              next();
            },
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
          import {useWorkerModels} from "/src/components/agents/WorkerModelPicker.tsx";
          function Fixture({account, workers, enabled}) {
            const result = useWorkerModels(account, enabled, workers);
            window.catalogResult = result;
            return React.createElement("pre", null, JSON.stringify(result));
          }
          window.mountCatalog = (account="first", workers=false, enabled=true) => {
            window.catalogRoot ||= createRoot(document.getElementById("app"));
            flushSync(() => window.catalogRoot.render(
              React.createElement(Fixture, {account, workers, enabled})));
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
    await page.evaluate(async () => {
      window.catalogReplies = [];
      window.catalogRequests = [];
      window.abortedCatalogs = 0;
      window.catalogRetryTimers = new Map();
      const nativeSetTimeout = window.setTimeout.bind(window);
      const nativeClearTimeout = window.clearTimeout.bind(window);
      let retryTimerSequence = 100000;
      window.setTimeout = (callback, delay, ...args) => {
        const stack = new Error().stack || "";
        if (
          delay !== 1000 ||
          !stack.includes("watchResourceReads.ts:") ||
          stack.includes("resourceEvents.ts:")
        )
          return nativeSetTimeout(callback, delay, ...args);
        const id = ++retryTimerSequence;
        window.catalogRetryTimers.set(id, {
          callback: () => callback(...args),
          stack,
        });
        return id;
      };
      window.clearTimeout = (id) => {
        window.catalogRetryTimers.delete(id);
        nativeClearTimeout(id);
      };
      window.runCatalogRetry = () => {
        const [id, callback] = window.catalogRetryTimers.entries().next().value;
        window.catalogRetryTimers.delete(id);
        callback.callback();
      };
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (url, options) => {
        const requestUrl = url instanceof Request ? url.url : String(url);
        if (
          new URL(requestUrl, window.location.href).pathname !== "/api/models"
        )
          return nativeFetch(url, options);
        window.catalogRequests.push(requestUrl);
        const reply = window.catalogReplies.shift();
        if (!reply) throw new Error("Missing catalog fixture reply");
        if (reply.hold) {
          const signal = url instanceof Request ? url.signal : options?.signal;
          await new Promise((resolve, reject) => {
            window.releaseCatalog = resolve;
            signal?.addEventListener(
              "abort",
              () => {
                window.abortedCatalogs++;
                reject(new DOMException("Aborted", "AbortError"));
              },
              { once: true },
            );
          });
        }
        if (reply.network) throw new TypeError("Failed to fetch");
        return new Response(JSON.stringify(reply.body), {
          status: reply.status || 200,
          headers: { "Content-Type": "application/json" },
        });
      };
      await import("/@id/__x00__virtual:model-catalog-pending");
    });
    const model = (value) => ({ model: value, displayName: value });
    const ready = (value) => ({ body: { data: [model(value)] } });
    const pending = {
      status: 400,
      body: { error: "Model catalog is pending", catalogPending: true },
    };
    const reset = async (replies, account = "first", workers = false) => {
      await page.evaluate(
        ({ replies, account, workers }) => {
          window.unmountCatalog();
          window.catalogReplies = replies;
          window.catalogRequests = [];
          window.mountCatalog(account, workers);
        },
        { replies, account, workers },
      );
      await page.waitForFunction(() => window.catalogRequests.length > 0);
    };
    const requestCount = () =>
      page.evaluate(() => window.catalogRequests.length);
    const retryTimerCount = () =>
      page.evaluate(() => window.catalogRetryTimers.size);
    const notifyResourceChange = async () => {
      const response = await fetch(
        new URL("/api/sync/notify", server.resolvedUrls.local[0]),
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            "X-Canvas-Token": "model-catalog-fixture",
            [API_SCHEMA_HASH_HEADER]: readApiSchemaHash(),
          },
          body: JSON.stringify({
            requestId: `catalog-change-${++resourceNotifySequence}`,
            resources: [{ kind: "models" }],
          }),
        },
      );
      expect(response.status).toBe(200);
      expect(await response.json()).toMatchObject({ accepted: true });
    };
    const emitResourceChange = async () => {
      const before = await requestCount();
      await notifyResourceChange();
      await page.waitForFunction(
        (before) => window.catalogRequests.length > before,
        before,
      );
    };
    const hasModel = (expected) =>
      page.waitForFunction(
        (expected) =>
          window.catalogResult.models.some((row) => row.model === expected),
        expected,
      );

    await reset([pending, ready("late-model")]);
    assert.equal(await page.evaluate(() => window.catalogResult.loading), true);
    assert.equal(await page.evaluate(() => window.catalogResult.error), "");
    assert.equal(
      await retryTimerCount(),
      0,
      "pending catalog waits for a model resource change",
    );
    await emitResourceChange();
    await hasModel("late-model");
    assert.equal(await retryTimerCount(), 0);

    const beforeRetry = await requestCount();
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
      (before) => window.catalogRequests.length > before,
      beforeRetry,
    );
    await hasModel("late-model");
    assert.equal(
      await page.evaluate(() => window.catalogResult.loading),
      false,
    );
    assert.equal(
      await retryTimerCount(),
      0,
      "explicitly retried pending catalog waits for a model resource change",
    );
    await emitResourceChange();
    await hasModel("updated-model");
    assert.deepEqual(
      await page.evaluate(() =>
        window.catalogResult.models.map((row) => row.model),
      ),
      ["updated-model"],
    );
    assert.equal(await retryTimerCount(), 0);

    await reset([ready("old-worker-model")], "first", true);
    await hasModel("old-worker-model");
    const beforeWorkerRetry = await requestCount();
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
    assert.equal(await requestCount(), beforeWorkerRetry + 1);
    assert.equal(
      await retryTimerCount(),
      0,
      "partial worker catalog waits for a model resource change",
    );
    await emitResourceChange();
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
    ];
    for (const reply of permanentReplies) {
      await page.evaluate((reply) => {
        window.catalogReplies = [reply];
        window.catalogResult.retry();
      }, reply);
      await page.waitForFunction(() => !!window.catalogResult.error);
      await hasModel("complete-worker-model");
      assert.equal(await retryTimerCount(), 0);
    }
    for (const reply of [
      { status: 503, body: { error: "Temporary failure" } },
      { network: true },
    ]) {
      const beforeRetryableFailure = await requestCount();
      const retainedModel = await page.evaluate(
        () => window.catalogResult.models[0]?.model,
      );
      await page.evaluate(
        ({ reply, recoveredReply }) => {
          window.catalogReplies = [reply, recoveredReply];
          window.catalogResult.retry();
        },
        { reply, recoveredReply: ready("recovered-model") },
      );
      await page.waitForFunction(
        (before) => window.catalogRequests.length > before,
        beforeRetryableFailure,
      );
      await page.waitForFunction(() => !!window.catalogResult.error);
      await hasModel(retainedModel);
      assert.equal(
        await retryTimerCount(),
        1,
        "retryable read failure schedules one watcher retry",
      );
      await page.evaluate(() => window.runCatalogRetry());
      await page.waitForFunction(
        (before) => window.catalogRequests.length > before + 1,
        beforeRetryableFailure,
      );
      await hasModel("recovered-model");
      await page.waitForFunction(() => window.catalogResult.error === "");
      assert.equal(await retryTimerCount(), 0);
    }
    for (const unavailableAccounts of [
      undefined,
      [],
      [{ accountKey: "second", error: "Pending", catalogPending: "true" }],
      [{ accountKey: "second", catalogPending: true }],
    ]) {
      await reset([
        {
          body: {
            data: [model("valid-model")],
            catalogPending: true,
            unavailableAccounts,
          },
        },
      ]);
      await hasModel("valid-model");
      assert.equal(
        await page.evaluate(() => window.catalogResult.loading),
        false,
      );
      assert.equal(await retryTimerCount(), 0);
    }

    await reset([pending, ready("must-not-load")]);
    await page.evaluate(() => window.mountCatalog("first", false, false));
    await notifyResourceChange();
    assert.equal(await page.evaluate(() => window.catalogRequests.length), 1);
    assert.equal(await retryTimerCount(), 0);

    await reset([{ ...ready("old-account-model"), hold: true }]);
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
    assert.equal(await page.evaluate(() => window.abortedCatalogs), 1);
    assert.deepEqual(errors, []);
    console.log(
      "PASS model catalog pending: cold and partial retry, old models, permanent errors, scope, cleanup",
    );
  } finally {
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
