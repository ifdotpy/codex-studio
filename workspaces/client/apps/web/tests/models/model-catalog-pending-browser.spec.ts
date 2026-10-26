import { createFixtureViteServer as createServer } from "../playwright.ts";
import { fixtureOrigin } from "../misc-fixture.ts";
import type {} from "./model-catalog-pending-browser.fixture.tsx";
// The model picker consumes streamed model entities while idle. Only its explicit
// retry action reads /api/models.

import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { apiSchemaHeaders, expect, test } from "../playwright.ts";

test("Model Catalog Entity Browser", async ({ page }) => {
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));

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
      },
    ],
    server: { host: "127.0.0.1", port: 0, hmr: false },
  });
  await server.listen();
  try {
    page.setDefaultTimeout(5000);
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/check", (route) =>
      route.fulfill({ contentType: "text/html", body: '<div id="app"></div>' }),
    );
    await page.goto(fixtureOrigin(server) + "/check");
    await page.evaluate(async (schemaHeaders) => {
      const model = (
        accountKey: string,
        name: string,
        hidden: boolean,
        catalogPosition: number,
        provider: string,
      ) => ({
        accountKey,
        model: name,
        hidden,
        catalogPosition,
        provider,
        serviceTiers: [],
        supportedReasoningEfforts: [],
      });
      window.modelCatalogFixture = {
        gets: 0,
        mount: () => {
          throw new Error("Catalog not mounted");
        },
        publish: () => {
          throw new Error("Catalog not mounted");
        },
        snapshot: {
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
        },
      };
      window.fetch = async (input, options) => {
        const request =
          input instanceof Request ? input : new Request(input, options);
        if (new URL(request.url).pathname === "/api/models") {
          window.modelCatalogFixture.gets++;
          return new Response(
            JSON.stringify({ data: [{ model: "refreshed-model" }] }),
            {
              headers: { "Content-Type": "application/json", ...schemaHeaders },
            },
          );
        }
        throw new Error(`Unexpected fetch: ${request.url}`);
      };
      const fixturePath =
        "/tests/models/model-catalog-pending-browser.fixture.tsx";
      await import(fixturePath);
    }, apiSchemaHeaders());

    await page.evaluate(() =>
      window.modelCatalogFixture.mount("default", false),
    );
    await expect
      .poll(() =>
        page.evaluate(() =>
          window.modelCatalogFixture.result?.models.map((row) => row.model),
        ),
      )
      .toEqual(["shared-model"]);
    await page.waitForTimeout(500);
    expect(await page.evaluate(() => window.modelCatalogFixture.gets)).toBe(0);

    await page.evaluate(() =>
      window.modelCatalogFixture.publish({
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
          window.modelCatalogFixture.result?.models.map((row) => row.model),
        ),
      )
      .toEqual(["updated-model"]);
    expect(await page.evaluate(() => window.modelCatalogFixture.gets)).toBe(0);

    await page.evaluate(() => window.modelCatalogFixture.result?.retry());
    await expect
      .poll(() => page.evaluate(() => window.modelCatalogFixture.gets))
      .toBe(1);
    await expect
      .poll(() =>
        page.evaluate(() =>
          window.modelCatalogFixture.result?.models.map((row) => row.model),
        ),
      )
      .toEqual(["refreshed-model"]);
    expect(errors).toEqual([]);
  } finally {
    await server.close();
    await rm(temporary, { recursive: true, force: true });
  }
});
