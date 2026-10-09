import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { spawnFixture as spawn, test, expect } from "../playwright.mjs";

test("worker workspace badge fits light, dark, and mobile chat headers", async ({
  page,
}, testInfo) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const state = await mkdtemp(join(tmpdir(), "studio-workspace-badge-"));
  const proc = spawn(
    "python3",
    [
      "-B",
      join(root, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      state,
    ],
    {
      env: { WORKSPACE_BADGE_FIXTURE: "1" },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  let log = "";
  proc.stderr.on("data", (data) => (log += data));
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    await page.emulateMedia({ colorScheme: "light" });
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(`http://127.0.0.1:${port}`);
    await page.locator("[data-chat]").first().waitFor();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await page.getByText("I assigned 40 workers", { exact: false }).waitFor();
    await page.locator("#team-toggle").click();
    await page.locator("#worker-search").fill("Worker 00");
    await page.locator("[data-worker]").click();
    const badge = page.locator(".workspace-mode-badge");
    await expect(badge).toHaveText("ASIF");
    await expect(badge).toHaveAttribute(
      "title",
      /Apple Sparse Image Format workspace.*workspace-badge/,
    );
    await page.screenshot({
      path: testInfo.outputPath("workspace-badge-light.png"),
    });

    await page.emulateMedia({ colorScheme: "dark" });
    await page.screenshot({
      path: testInfo.outputPath("workspace-badge-dark.png"),
    });

    await page.emulateMedia({ colorScheme: "light" });
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(badge).toBeVisible();
    const bounds = await badge.boundingBox();
    assert.ok(bounds && bounds.x >= 0 && bounds.x + bounds.width <= 390);
    await page.screenshot({
      path: testInfo.outputPath("workspace-badge-mobile.png"),
    });
  } finally {
    if (proc.exitCode === null && proc.signalCode === null) {
      proc.stdin.end();
      proc.kill("SIGTERM");
    }
  }
});
