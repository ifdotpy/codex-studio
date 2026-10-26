import { required } from "../teams/helpers.ts";
import { createServer } from "vite";
import type {} from "./workspace-identity-retry-browser.fixture.tsx";
// A failed workspace identity read at startup must not refuse attachments until a restart.
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  API_SCHEMA_HASH_HEADER,
  expect,
  readApiSchemaHash,
  test,
} from "../playwright.ts";

test("workspace identity retry browser", async ({ browser }) => {
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const cache = await mkdtemp(join(tmpdir(), "studio-identity-retry-"));
  const server = await createServer({
    configFile: false,
    root: join(repo, "workspaces/client/apps/web"),
    cacheDir: cache,
    optimizeDeps: { include: ["react", "react-dom/client"] },
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "identity-fixture",
      },
    ],
  });
  let context;
  try {
    await server.listen();
    context = await browser.newContext();
    const page = await context.newPage();
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const workspaceId = "c".repeat(32);
    let identityReads = 0;
    await page.route("**/check", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<!doctype html><title>Identity</title>",
      }),
    );
    // The first read fails with a status that is not a network error, and this
    // device has no cached workspace to fall back to.
    await page.route("**/api/sync/identity", (route) =>
      ++identityReads === 1
        ? route.fulfill({ status: 403, json: { error: "Origin not ready" } })
        : route.fulfill({
            headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
            json: { workspaceId, syncProtocol: 2 },
          }),
    );
    await page.route("**/api/**", (route) =>
      route.request().url().includes("/api/sync/identity")
        ? route.fallback()
        : route.fulfill({ status: 503, json: { error: "Not in this test" } }),
    );
    const url = required(server.resolvedUrls?.local[0]);
    await page.goto(url + "check");
    await page.addScriptTag({
      type: "module",
      url: "/tests/files/workspace-identity-retry-browser.fixture.tsx",
    });
    await page.waitForFunction(
      () => !!window.workspaceIdentityRetryBrowserMount,
    );
    await page.evaluate(() => window.workspaceIdentityRetryBrowserMount());
    await expect.poll(() => identityReads).toBeGreaterThan(1);
    await expect
      .poll(() =>
        page.evaluate(() => window.workspaceIdentityRetryBrowserWorkspace),
      )
      .toBe(workspaceId);
    assert.deepEqual(errors, []);
  } finally {
    await context?.close();
    await server.close();
    await rm(cache, { recursive: true, force: true });
  }
});
