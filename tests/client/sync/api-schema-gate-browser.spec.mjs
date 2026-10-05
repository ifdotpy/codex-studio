import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  expect,
  readApiSchemaHash,
  spawnFixture,
  test,
} from "../playwright.mjs";

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

    let totalRequests = 0;
    let apiRequests = 0;
    let mutationRequests = 0;
    const syncRequests = [];
    page.on("request", (request) => {
      totalRequests++;
      const url = new URL(request.url());
      if (url.pathname.startsWith("/api/")) apiRequests++;
      if (request.method() !== "GET" && url.pathname.startsWith("/api/"))
        mutationRequests++;
      if (url.pathname.startsWith("/api/sync/"))
        syncRequests.push(request.url());
    });
    let mismatchEnabled = true;
    let schemaResponses = 0;
    let workerMode = "normal";
    let workerFetches = 0;
    await page.route("**/studio-sw.js", async (route) => {
      workerFetches++;
      if (workerMode !== "changed") return route.continue();
      const response = await route.fetch();
      const body = `${await response.text()}\n// schema-gate-worker-update-probe\n`;
      await route.fulfill({ response, body });
    });
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
    const settledRequests = totalRequests;
    const settledSyncRequests = syncRequests.length;
    const settledMutations = mutationRequests;
    await page.waitForTimeout(8_000);
    assert.equal(
      totalRequests,
      settledRequests,
      "No network requests follow the mismatch alert",
    );
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

    await page.evaluate(async () => {
      const registration = await navigator.serviceWorker.getRegistration("/");
      await registration?.unregister();
    });
    await page.getByRole("button", { name: "Update" }).click();
    await expect(
      page.locator('[data-modal-content="true"]').getByRole("alert"),
    ).toContainText("not registered", { timeout: 30_000 });
    expect(
      await page.evaluate(() =>
        sessionStorage.getItem("studio-api-schema-update-attempted"),
      ),
    ).toBeNull();
    await page.evaluate(async () => {
      await navigator.serviceWorker.register("/studio-sw.js", { scope: "/" });
      await navigator.serviceWorker.ready;
    });
    await page.reload();
    await expect
      .poll(() => page.evaluate(() => !!navigator.serviceWorker?.controller))
      .toBe(true);
    workerMode = "changed";
    await page.evaluate(async () => {
      const registration = await navigator.serviceWorker.getRegistration("/");
      registration?.addEventListener("updatefound", () => {
        const worker = registration.installing;
        worker?.addEventListener("statechange", () => {
          if (worker.state === "activated")
            sessionStorage.setItem("schema-gate-worker-activated", "1");
        });
      });
    });
    await page.getByRole("button", { name: "Update" }).click();
    await expect(page).toHaveURL(/studio-update=/, { timeout: 30_000 });
    expect(workerFetches).toBeGreaterThan(0);
    await expect(page).not.toHaveURL(/studio-update=/, { timeout: 30_000 });
    await expect
      .poll(() =>
        page.evaluate(() =>
          sessionStorage.getItem("schema-gate-worker-activated"),
        ),
      )
      .toBe("1");
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
        body: `event: api-schema\ndata: ${JSON.stringify({ hash: readApiSchemaHash(), mismatch: true })}\n\n`,
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

test("a stream mismatch keeps the transcript and draft while stopping every request", async ({
  page,
}) => {
  const root = fileURLToPath(new URL("../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-schema-stream-gate-"));
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
    await page.evaluate(async () => {
      await navigator.serviceWorker?.ready;
    });
    await page.reload();
    await page.evaluate(() => {
      localStorage.setItem("schema-stream-gate-ready", "1");
    });
    await page
      .getByRole("button", { name: /^Release lead/ })
      .first()
      .click();
    await page.waitForFunction(
      () => document.querySelectorAll("article[data-message]").length > 0,
    );
    const visibleMessages = await page.locator("article[data-message]").count();
    await page.locator("#message").fill("Draft survives a stream mismatch");
    await page.waitForFunction(() =>
      Object.keys(localStorage).some((key) =>
        localStorage.getItem(key)?.includes("Draft survives a stream mismatch"),
      ),
    );
    let totalRequests = 0;
    page.on("request", () => totalRequests++);
    await page.route("**/api/sync/stream**", async (route) => {
      await route.fulfill({
        status: 200,
        headers: { "content-type": "text/event-stream" },
        body: `event: api-schema\ndata: ${JSON.stringify({ hash: readApiSchemaHash(), mismatch: true })}\n\n`,
      });
    });
    await page.reload();
    await expect(
      page.locator('[data-modal-content="true"]').filter({
        hasText: "Studio has been updated. Update this tab",
      }),
    ).toBeVisible();
    await expect(page.locator("#message")).toHaveValue(
      "Draft survives a stream mismatch",
    );
    await expect(page.locator("article[data-message]")).toHaveCount(
      visibleMessages,
    );
    await expect(
      page.getByRole("button", { name: "Send message" }),
    ).toBeDisabled();
    const settledRequests = totalRequests;
    await page.waitForTimeout(8_000);
    assert.equal(
      totalRequests,
      settledRequests,
      "No requests follow a stream mismatch",
    );
  } finally {
    fixture.kill();
  }
});
