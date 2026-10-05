import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { expect, spawnFixture, test } from "../playwright.mjs";

test("a response mismatch keeps the loaded transcript and local draft while stopping sync", async ({
  page,
}) => {
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-schema-gate-"));
  const fixture = spawnFixture(
    process.env.PYTHON_BIN || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), state],
    { stdio: ["pipe", "pipe", "pipe"], env: { ...process.env } },
  );
  let fixtureLog = "";
  fixture.stderr.on("data", (chunk) => {
    fixtureLog += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(fixtureLog)));
    });
    await page.goto(`http://127.0.0.1:${port}`);
    await page
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .click();
    await page.waitForFunction(
      () => document.querySelectorAll("article[data-message]").length > 0,
    );
    const visibleMessages = await page.locator("article[data-message]").count();
    await page.locator("#message").fill("Keep this unsent draft");
    await page.waitForFunction(() =>
      Object.keys(localStorage).some((key) =>
        localStorage.getItem(key)?.includes("Keep this unsent draft"),
      ),
    );

    let mutationRequests = 0;
    const syncRequests = [];
    let seenSchemaHeader;
    await page.evaluate(() => {
      window.__schemaMismatchEvent = false;
      window.addEventListener("studio-api-schema-mismatch", () => {
        window.__schemaMismatchEvent = true;
      });
    });
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (request.method() !== "GET" && url.pathname.startsWith("/api/"))
        mutationRequests++;
      if (url.pathname.startsWith("/api/sync/"))
        syncRequests.push(request.url());
    });
    page.on("response", async (response) => {
      if (new URL(response.url()).pathname === "/api/session")
        seenSchemaHeader = (await response.allHeaders())["x-studio-api-schema"];
    });
    let schemaStreamRequests = 0;
    await page.route("**/api/sync/stream**", async (route) => {
      schemaStreamRequests++;
      await route.fulfill({
        status: 503,
        headers: {
          "content-type": "application/json",
        },
        body: '{"detail":"test stream unavailable"}',
      });
    });
    let schemaResponses = 0;
    await page.route("**/api/session", async (route) => {
      const response = await route.fetch();
      schemaResponses++;
      await route.fulfill({
        response,
        headers: {
          ...response.headers(),
          "x-studio-api-schema": "foreign-schema",
        },
      });
    });
    await page.evaluate(() => {
      window.dispatchEvent(new Event("offline"));
      window.dispatchEvent(new Event("online"));
    });
    await expect.poll(() => schemaStreamRequests).toBeGreaterThan(0);
    await expect.poll(() => schemaResponses).toBeGreaterThan(0);
    await expect.poll(() => seenSchemaHeader).toBe("foreign-schema");
    await expect
      .poll(() => page.evaluate(() => window.__schemaMismatchEvent))
      .toBe(true);
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible();
    await expect(page.locator("#message")).toHaveValue(
      "Keep this unsent draft",
    );
    await expect(
      page.getByRole("button", { name: "Send message" }),
    ).toBeDisabled();
    await expect(page.locator("article[data-message]")).toHaveCount(
      visibleMessages,
    );
    const settledSyncRequests = syncRequests.length;
    const settledMutations = mutationRequests;
    await page.waitForTimeout(750);
    assert.equal(
      syncRequests.length,
      settledSyncRequests,
      "No sync requests follow the mismatch alert",
    );
    assert.equal(
      mutationRequests,
      settledMutations,
      "No mutations follow the mismatch alert",
    );
  } finally {
    fixture.kill();
  }
});
