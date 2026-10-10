import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect, spawnFixture as spawn } from "../playwright.mjs";

test("Snapshot Network Recovery Browser", async ({
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

  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const evidence = await mkdtemp(join(tmpdir(), "studio-snapshot-network-"));
  const fixture = spawn(
    process.env.PYTHON_BIN || "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      evidence,
    ],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        TOKEN_RATE_WORKER_COUNT: "1",
        CODEX_BOARD_STATE_DIR: join(evidence, "board"),
      },
    },
  );
  let log = "";
  fixture.stderr.on("data", (data) => (log += data));
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (data) =>
        resolve(Number(String(data).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const page = testPage;
    let failure = null;
    let requests = 0;
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/session", async (route) => {
      requests++;
      if (failure === "network") return route.abort("failed");
      if (failure === "503")
        return route.fulfill({
          status: 503,
          json: { error: "Server unavailable" },
        });
      if (failure === "403")
        return route.fulfill({ status: 403, json: { error: "Access denied" } });
      return route.continue();
    });
    await page.goto(`http://127.0.0.1:${port}`);
    await page.getByText("Release lead", { exact: true }).first().waitFor();
    await page.locator("#error").waitFor({ state: "hidden" });
    failure = "network";
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await page
      .locator("#error")
      .getByText("Failed to fetch", { exact: true })
      .waitFor();
    const failedAt = Date.now();
    failure = null;
    // No online, visibility or click event helps this recovery.
    await page.locator("#error").waitFor({ state: "hidden", timeout: 5000 });
    assert.ok(Date.now() - failedAt < 5000);
    const recoveredRequests = requests;
    await page.waitForTimeout(1500);
    assert.equal(
      requests,
      recoveredRequests,
      "Healthy sessions do not poll faster.",
    );
    failure = "503";
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await page
      .locator("#error")
      .getByText("Server unavailable", { exact: true })
      .waitFor();
    failure = null;
    await page.locator("#error").waitFor({ state: "hidden", timeout: 5000 });
    failure = "403";
    await page.evaluate(() => window.dispatchEvent(new Event("online")));
    await page
      .locator("#error")
      .getByText("Access denied", { exact: true })
      .waitFor();
    const deniedRequests = requests;
    await page.waitForTimeout(1500);
    assert.equal(
      requests,
      deniedRequests,
      "A permission rejection does not authorize a fast retry.",
    );
    assert.deepEqual(errors, []);
    console.log(
      `PASS: the global network error clears without user input; healthy and denied sessions do not repeat reads. ${evidence}`,
    );
  } finally {
    // spawnFixture is cleaned by the shared Playwright test fixture.
  }
});
