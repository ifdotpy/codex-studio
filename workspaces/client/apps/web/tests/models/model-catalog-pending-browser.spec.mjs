// The model picker consumes streamed model entities while idle. Only its explicit
// retry action reads /api/models.

import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { apiSchemaHeaders, expect, test } from "../playwright.mjs";

test("Model Catalog Entity Browser", async ({ page }) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const require = createRequire(
    join(root, "workspaces/client/apps/web/package.json"),
  );
  const { createServer } = await import(require.resolve("vite"));
  const temporary = await mkdtemp(join(tmpdir(), "studio-model-entities-"));
  const server = await createServer({
    configFile: false,
    root: join(root, "workspaces/client/apps/web"),
    cacheDir: temporary,
    optimizeDeps: {
      include: ["react", "react-dom/client", "react/jsx-dev-runtime"],
    },
    plugins: [
      {
        name: "model-entity-harness",
        resolveId(id) {
          if (id === "virtual:model-entity-harness") return "\0" + id;
        },
        load(id) {
          if (id !== "\0virtual:model-entity-harness") return;
          return `
            import React from "react";
            import {createRoot} from "react-dom/client";
            import {flushSync} from "react-dom";
            import {ModelCatalogReader, useWorkerModels} from "/src/components/agents/WorkerModelPicker.tsx";
            import {get} from "/src/api.ts";
            const reader = {
              scope: "fixture",
              cacheOwner: {},
              read: options => get("/api/models", options),
              subscribeState: accept => {
                window.acceptModelSnapshot = accept;
                accept(window.modelSnapshot);
                return () => { window.acceptModelSnapshot = undefined; };
              }
            };
            function Fixture({account, workers}) {
              const result = useWorkerModels(account, true, workers);
              window.catalogResult = result;
              return React.createElement("pre", null, JSON.stringify(result));
            }
            window.mountCatalog = (account="default", workers=false) => {
              window.catalogRoot ||= createRoot(document.getElementById("app"));
              flushSync(() => window.catalogRoot.render(
                React.createElement(ModelCatalogReader.Provider, {value: reader},
                  React.createElement(Fixture, {account, workers}))));
            };
            window.publishModelSnapshot = snapshot => {
              window.modelSnapshot = snapshot;
              window.acceptModelSnapshot?.(snapshot);
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
    page.setDefaultTimeout(5000);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({ contentType: "text/html", body: '<div id="app"></div>' }),
    );
    await page.goto(server.resolvedUrls.local[0] + "check");
    await page.evaluate(async (schemaHeaders) => {
      const model = (accountKey, name, hidden, catalogPosition, provider) => ({
        accountKey,
        model: name,
        hidden,
        catalogPosition,
        provider,
        serviceTiers: [],
        supportedReasoningEfforts: [],
      });
      window.modelSnapshot = {
        entitiesComplete: true,
        runtime: {
          modelsByAccount: {
            default: [
              model("default", "shared-model", false, 0, "codex"),
              model("default", "hidden-model", true, 1, "codex"),
            ],
            secondary: [
              model("secondary", "other-model", false, 0, "claude"),
              model("secondary", "shared-model", false, 1, "claude"),
            ],
          },
        },
      };
      window.modelGets = 0;
      window.fetch = async (input, options) => {
        const request =
          input instanceof Request ? input : new Request(input, options);
        if (new URL(request.url).pathname === "/api/models") {
          window.modelGets++;
          return new Response(
            JSON.stringify({ data: [{ model: "refreshed-model" }] }),
            {
              headers: { "Content-Type": "application/json", ...schemaHeaders },
            },
          );
        }
        throw new Error(`Unexpected fetch: ${request.url}`);
      };
      await import("/@id/__x00__virtual:model-entity-harness");
    }, apiSchemaHeaders());

    await page.evaluate(() => window.mountCatalog("default", false));
    await expect
      .poll(() =>
        page.evaluate(() =>
          window.catalogResult.models.map((row) => row.model),
        ),
      )
      .toEqual(["shared-model"]);
    await page.waitForTimeout(500);
    expect(await page.evaluate(() => window.modelGets)).toBe(0);

    await page.evaluate(() =>
      window.publishModelSnapshot({
        entitiesComplete: true,
        runtime: {
          modelsByAccount: {
            default: [
              {
                accountKey: "default",
                model: "updated-model",
                hidden: false,
                catalogPosition: 0,
                provider: "codex",
              },
            ],
          },
        },
      }),
    );
    await expect
      .poll(() =>
        page.evaluate(() =>
          window.catalogResult.models.map((row) => row.model),
        ),
      )
      .toEqual(["updated-model"]);
    expect(await page.evaluate(() => window.modelGets)).toBe(0);

    await page.evaluate(() => window.catalogResult.retry());
    await expect.poll(() => page.evaluate(() => window.modelGets)).toBe(1);
    await expect
      .poll(() =>
        page.evaluate(() =>
          window.catalogResult.models.map((row) => row.model),
        ),
      )
      .toEqual(["refreshed-model"]);
    expect(errors).toEqual([]);
  } finally {
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
