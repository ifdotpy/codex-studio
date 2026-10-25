import assert from "node:assert/strict";
import { test, expect } from "../playwright.mjs";

test("Snapshot Network Recovery Browser", async ({ page, readOnlyRust }) => {
  let failure = null;
  let requests = 0;
  let failedNetworkRequests = 0;
  const activeSessionRequests = new Set();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    if (new URL(request.url()).pathname !== "/api/session") return;
    activeSessionRequests.add(request);
  });
  const finishSessionRequest = (request) =>
    activeSessionRequests.delete(request);
  page.on("requestfinished", finishSessionRequest);
  page.on("requestfailed", finishSessionRequest);
  const waitForSessionRequestsToSettle = () =>
    expect
      .poll(() => activeSessionRequests.size, {
        message:
          "in-flight session reads settle before measuring the next interval",
      })
      .toBe(0);
  await page.route("**/api/session", async (route) => {
    requests++;
    if (failure === "network") {
      failedNetworkRequests++;
      return route.abort("failed");
    }
    if (failure === "503")
      return route.fulfill({
        status: 503,
        json: { error: "Server unavailable" },
      });
    if (failure === "403")
      return route.fulfill({ status: 403, json: { error: "Access denied" } });
    return route.continue();
  });
  await page.clock.install();
  await readOnlyRust.open(page);
  const recoveryWindowMs = 5_000;
  await page.getByText("Release lead", { exact: true }).first().waitFor();
  await page.locator("#error").waitFor({ state: "hidden" });
  await waitForSessionRequestsToSettle();
  failure = "network";
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await page
    .locator("#error")
    .getByText("Failed to fetch", { exact: true })
    .waitFor();
  const failedAt = Date.now();
  failure = null;
  // Only the retry timer advances; no lifecycle event helps recovery.
  await page.clock.runFor(recoveryWindowMs);
  await page.locator("#error").waitFor({ state: "hidden", timeout: 5000 });
  assert.ok(Date.now() - failedAt < 5000);
  await waitForSessionRequestsToSettle();
  assert.ok(
    failedNetworkRequests > 0,
    "the fixture fails every session read during the recovery error state",
  );
  failure = "503";
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await page
    .locator("#error")
    .getByText("Server unavailable", { exact: true })
    .waitFor();
  failure = null;
  await page.clock.runFor(recoveryWindowMs);
  await page.locator("#error").waitFor({ state: "hidden", timeout: 5000 });
  failure = "403";
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await page
    .locator("#error")
    .getByText("Access denied", { exact: true })
    .waitFor();
  await waitForSessionRequestsToSettle();
  const deniedRequests = requests;
  await page.clock.runFor(recoveryWindowMs);
  assert.equal(
    requests,
    deniedRequests,
    "A permission rejection does not authorize a fast retry.",
  );
  assert.deepEqual(errors, []);
  console.log(
    `PASS: the global network error clears without user input; healthy and denied sessions do not repeat reads.`,
  );
});
