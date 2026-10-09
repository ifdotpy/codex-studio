import assert from "node:assert/strict";

import {
  API_SCHEMA_HASH_HEADER,
  expect,
  readApiSchemaHash,
  test,
} from "./playwright.mjs";

async function assertOneApiFulfillment(page, explicitHash) {
  let apiFulfillments = 0;
  await page.route("http://schema-header-fixture.test/**", async (route) => {
    const url = new URL(route.request().url());
    if (!url.pathname.startsWith("/api/"))
      return route.fulfill({ contentType: "text/html", body: "fixture" });
    apiFulfillments++;
    const headers = explicitHash
      ? { [API_SCHEMA_HASH_HEADER]: explicitHash }
      : {};
    await route.fulfill({ status: 200, headers, body: "ok" });
  });
  await page.goto("http://schema-header-fixture.test/");
  const receivedHash = await page.evaluate(async () => {
    const response = await fetch("/api/probe");
    return response.headers.get("x-studio-api-schema");
  });
  assert.equal(
    apiFulfillments,
    1,
    "the spec route fulfills the API request once",
  );
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

test("deferred route fulfillment keeps the schema decorator until handled", async ({
  page,
}) => {
  let heldRoute;
  let fulfillRoute;
  const routeCaptured = new Promise((resolve) => {
    fulfillRoute = resolve;
  });
  let apiFulfillments = 0;
  await page.route("http://schema-header-fixture.test/**", async (route) => {
    if (!new URL(route.request().url()).pathname.startsWith("/api/"))
      return route.fulfill({ contentType: "text/html", body: "fixture" });
    apiFulfillments++;
    heldRoute = route;
    fulfillRoute();
  });
  await page.goto("http://schema-header-fixture.test/");
  const responseHash = page.evaluate(async () => {
    const response = await fetch("/api/deferred");
    return response.headers.get("x-studio-api-schema");
  });
  await routeCaptured;
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
