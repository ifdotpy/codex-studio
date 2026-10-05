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
    await expect(page.getByRole("alertdialog")).toHaveCount(0);
    await page.evaluate(async () => {
      await navigator.serviceWorker?.ready;
    });
    await page.reload();
    await expect
      .poll(() => page.evaluate(() => !!navigator.serviceWorker?.controller))
      .toBe(true);
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

    let apiRequests = 0;
    let mutationRequests = 0;
    const syncRequests = [];
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (url.pathname.startsWith("/api/")) apiRequests++;
      if (request.method() !== "GET" && url.pathname.startsWith("/api/"))
        mutationRequests++;
      if (url.pathname.startsWith("/api/sync/"))
        syncRequests.push(request.url());
    });
    let mismatchEnabled = true;
    let schemaResponses = 0;
    await page.route("**/api/**", async (route) => {
      if (new URL(route.request().url()).pathname === "/api/sync/stream")
        return route.continue();
      if (!mismatchEnabled) return route.continue();
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
    await page.reload();
    await expect.poll(() => schemaResponses).toBeGreaterThan(0);
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
    const settledApiRequests = apiRequests;
    const settledSyncRequests = syncRequests.length;
    const settledMutations = mutationRequests;
    await page.waitForTimeout(8_000);
    assert.equal(
      apiRequests,
      settledApiRequests,
      "No API requests follow the mismatch alert",
    );
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

    await page.getByRole("button", { name: "Update" }).click();
    await expect(page).toHaveURL(/studio-update=/, { timeout: 30_000 });
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "The installed renderer build does not match the server",
      }),
    ).toBeVisible({ timeout: 30_000 });
    await expect(page.getByRole("button", { name: "Update" })).toHaveCount(0);

    mismatchEnabled = false;
    await page.reload();
    await expect(page.getByRole("alertdialog")).toHaveCount(0);
    expect(
      await page.evaluate(() =>
        sessionStorage.getItem("studio-api-schema-update-attempted"),
      ),
    ).toBeNull();

    await page.route("**/api/sync/stream**", async (route) => {
      await route.fulfill({
        status: 200,
        headers: { "content-type": "text/event-stream" },
        body: 'event: api-schema\ndata: {"hash":"foreign-schema"}\n\n',
      });
    });
    await page.reload();
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible();
  } finally {
    fixture.kill();
  }
});
