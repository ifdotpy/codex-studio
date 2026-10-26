import { build } from "esbuild";
import { fileURLToPath } from "node:url";
import type { Page, Route } from "@playwright/test";
import type {} from "./playwright-schema-headers.fixture.ts";
import assert from "node:assert/strict";

import {
  API_SCHEMA_HASH_HEADER,
  expect,
  readApiSchemaHash,
  test,
} from "./playwright.ts";

async function openFixture(page: Page) {
  await page.goto("http://schema-header-fixture.test/");
  const bundle = await build({
    entryPoints: [
      fileURLToPath(
        new URL("./playwright-schema-headers.fixture.ts", import.meta.url),
      ),
    ],
    bundle: true,
    write: false,
  });
  const output = bundle.outputFiles[0];
  if (!output) throw new Error("Missing schema header fixture bundle");
  await page.addScriptTag({ content: output.text });
}

async function assertOneApiFulfillment(page: Page, explicitHash?: string) {
  let apiHandlerCalls = 0;
  let apiFulfillCalls = 0;
  let apiResponses = 0;
  page.on("response", (response) => {
    if (new URL(response.url()).pathname === "/api/probe") apiResponses++;
  });
  await page.route("http://schema-header-fixture.test/**", async (route) => {
    const url = new URL(route.request().url());
    if (!url.pathname.startsWith("/api/"))
      return route.fulfill({ contentType: "text/html", body: "fixture" });
    apiHandlerCalls++;
    const headers = explicitHash
      ? { [API_SCHEMA_HASH_HEADER]: explicitHash }
      : {};
    apiFulfillCalls++;
    await route.fulfill({ status: 200, headers, body: "ok" });
  });
  await openFixture(page);
  const receivedHash = await page.evaluate(
    async () => (await window.schemaHeaderFixture.probe("/api/probe")).hash,
  );
  assert.equal(apiHandlerCalls, 1, "the spec API route handler runs once");
  assert.equal(apiFulfillCalls, 1, "the spec calls fulfill exactly once");
  assert.equal(apiResponses, 1, "the browser receives one API response");
  return receivedHash;
}

test("schema fixture headers decorate one spec fulfillment by default", async ({
  page,
}) => {
  expect(await assertOneApiFulfillment(page)).toBe(readApiSchemaHash());
});

test("explicit schema fixture response hash takes precedence", async ({
  page,
}) => {
  const explicitHash = "a".repeat(64);
  expect(await assertOneApiFulfillment(page, explicitHash)).toBe(explicitHash);
});

test("duplicate spec fulfill calls delegate to one Route fulfillment", async ({
  page,
}) => {
  let apiHandlerCalls = 0;
  let apiResponses = 0;
  page.on("response", (response) => {
    if (new URL(response.url()).pathname === "/api/probe") apiResponses++;
  });
  await page.route("http://schema-header-fixture.test/**", async (route) => {
    if (!new URL(route.request().url()).pathname.startsWith("/api/"))
      return route.fulfill({ contentType: "text/html", body: "fixture" });
    apiHandlerCalls++;
    const fulfillment = route.fulfill({ status: 200, body: "ok" });
    await Promise.all([
      fulfillment,
      route.fulfill({ status: 200, body: "duplicate" }),
    ]);
  });
  await openFixture(page);
  const response = await page.evaluate(() =>
    window.schemaHeaderFixture.probe("/api/probe"),
  );
  assert.equal(apiHandlerCalls, 1, "the spec route handles the request once");
  assert.equal(apiResponses, 1, "the browser receives one response");
  assert.equal(response.body, "ok", "the first spec fulfillment wins");
  assert.equal(response.hash, readApiSchemaHash());
});

test("deferred route fulfillment keeps the schema decorator until handled", async ({
  page,
}) => {
  let captureRoute: (route: Route) => void = () => {
    throw new Error("Route capture not initialized");
  };
  const routeCaptured = new Promise<Route>((resolve) => {
    captureRoute = resolve;
  });
  let apiFulfillments = 0;
  await page.route("http://schema-header-fixture.test/**", async (route) => {
    if (!new URL(route.request().url()).pathname.startsWith("/api/"))
      return route.fulfill({ contentType: "text/html", body: "fixture" });
    apiFulfillments++;
    captureRoute(route);
  });
  await openFixture(page);
  const responseHash = page.evaluate(
    async () => (await window.schemaHeaderFixture.probe("/api/deferred")).hash,
  );
  const heldRoute = await routeCaptured;
  await heldRoute.fulfill({ status: 200, body: "ok" });
  expect(await responseHash).toBe(readApiSchemaHash());
  assert.equal(apiFulfillments, 1, "the deferred spec route fulfills once");
});

test.describe("schema fixture header opt-out", () => {
  test.use({ apiSchemaFixtureHeaders: false });

  test("one spec fulfillment remains without an injected schema hash", async ({
    page,
  }) => {
    expect(await assertOneApiFulfillment(page)).toBeNull();
  });
});
