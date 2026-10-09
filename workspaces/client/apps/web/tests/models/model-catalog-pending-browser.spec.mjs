// Actual catalog hook and API error handling. No backend or native model calls.

import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.mjs";

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
      window.catalogTimers = new Map();
      window.abortedCatalogs = 0;
      const nativeSetTimeout = window.setTimeout.bind(window);
      const nativeClearTimeout = window.clearTimeout.bind(window);
      let timerSequence = 100000;
      window.setTimeout = (callback, delay, ...args) => {
        if (delay !== 1000) return nativeSetTimeout(callback, delay, ...args);
        const id = ++timerSequence;
        window.catalogTimers.set(id, () => callback(...args));
        return id;
      };
      window.clearTimeout = (id) => {
        window.catalogTimers.delete(id);
        nativeClearTimeout(id);
      };
      window.runCatalogRetry = () => {
        const [id, callback] = window.catalogTimers.entries().next().value;
        window.catalogTimers.delete(id);
        callback();
      };
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (url, options) => {
        if (!String(url).startsWith("/api/models?"))
          return nativeFetch(url, options);
        window.catalogRequests.push(String(url));
        const reply = window.catalogReplies.shift();
        if (!reply) throw new Error("Missing catalog fixture reply");
        if (reply.hold)
          await new Promise((resolve, reject) => {
            window.releaseCatalog = resolve;
            options?.signal?.addEventListener(
              "abort",
              () => {
                window.abortedCatalogs++;
                reject(new DOMException("Aborted", "AbortError"));
              },
              { once: true },
            );
          });
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
    const timers = () => page.evaluate(() => window.catalogTimers.size);
    const retry = async () => {
      const before = await page.evaluate(() => window.catalogRequests.length);
      await page.evaluate(() => window.runCatalogRetry());
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
    await page.waitForFunction(() => window.catalogTimers.size === 1);
    assert.equal(await page.evaluate(() => window.catalogResult.loading), true);
    assert.equal(await page.evaluate(() => window.catalogResult.error), "");
    await retry();
    await hasModel("late-model");
    assert.equal(await timers(), 0);

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
    await page.waitForFunction(() => window.catalogTimers.size === 1);
    await hasModel("late-model");
    assert.equal(
      await page.evaluate(() => window.catalogResult.loading),
      false,
    );
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
    assert.equal(await timers(), 1);
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
      assert.equal(await timers(), 0);
      await hasModel("complete-worker-model");
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
      assert.equal(await timers(), 0);
    }

    await reset([pending, ready("must-not-load")]);
    await page.waitForFunction(() => window.catalogTimers.size === 1);
    await page.evaluate(() => window.mountCatalog("first", false, false));
    assert.equal(await timers(), 0);
    assert.equal(await page.evaluate(() => window.catalogRequests.length), 1);

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
