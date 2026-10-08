import { test, expect, spawnFixture } from "../playwright.mjs";
import { chooseSetupValue } from "../../setup-controls.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("new chat chips open each role, save settings and disappear after send", async ({
  page,
}) => {
  test.setTimeout(120000);
  const root = join(import.meta.dirname, "../../..");
  const evidence = await mkdtemp(join(tmpdir(), "studio-new-chat-settings-"));
  const fixture = spawnFixture(
    "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), evidence],
    {
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(`http://127.0.0.1:${port}`);
    await page.locator("#message").waitFor();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .first()
      .click();
    await page
      .locator(".sidebar-nav")
      .getByRole("button", { name: "New chat", exact: true })
      .click();
    const row = page.locator(".empty-chat-settings");
    await expect(row).toBeVisible();
    await expect(row.locator(".execution-menu")).toHaveCount(2);
    await expect(page.locator("#composer .execution-menu")).toHaveCount(0);
    await expect(page.locator(".empty-chat p")).toHaveCount(0);
    const main = row.getByRole("button", {
      name: "Main agent settings",
      exact: true,
    });
    const workers = row.getByRole("button", {
      name: "Subagent defaults",
      exact: true,
    });
    await expect(main).toContainText(/ · .+ · .+/);
    await expect(workers).toContainText(/Luna 6 · High · .+/);
    const composerBounds = await page.locator("#composer").boundingBox();
    const rowBounds = await row.boundingBox();
    expect(rowBounds.y).toBeGreaterThanOrEqual(
      composerBounds.y + composerBounds.height,
    );
    await main.click();
    await expect(
      page.getByRole("button", { name: "Orchestrator", exact: true }),
    ).toHaveAttribute("aria-pressed", "true");
    const mainSave = page.waitForResponse(
      (response) =>
        response.url().endsWith("/api/conversation") &&
        response.request().postDataJSON()?.effort === "high",
    );
    await chooseSetupValue(
      page.getByLabel("Main agent reasoning", { exact: true }),
      "high",
    );
    expect((await mainSave).ok()).toBe(true);
    await page.keyboard.press("Escape");
    await expect(main).toContainText(" · High · ");
    await workers.click();
    await expect(
      page.getByRole("button", { name: "Worker", exact: true }),
    ).toHaveAttribute("aria-pressed", "true");
    const workerSave = page.waitForResponse(
      (response) =>
        response.url().endsWith("/api/conversation") &&
        response.request().postDataJSON()?.worker_defaults?.effort === "medium",
    );
    await chooseSetupValue(
      page.getByLabel("Default subagent reasoning", { exact: true }),
      "medium",
    );
    expect((await workerSave).ok()).toBe(true);
    await page.keyboard.press("Escape");
    await expect(workers).toContainText(" · Medium · ");
    await page.reload();
    await expect(main).toContainText(" · High · ");
    await expect(workers).toContainText(" · Medium · ");
    await page.setViewportSize({ width: 390, height: 844 });
    for (const chip of [main, workers]) {
      await expect(chip).toBeVisible();
      expect(
        await chip
          .locator(".execution-selected")
          .evaluate((node) => node.scrollWidth <= node.clientWidth),
      ).toBe(true);
    }
    await page.locator("#message").fill("Fixture first message");
    const send = page.waitForResponse(
      (response) =>
        response.url().endsWith("/api/messages") &&
        response.request().method() === "POST",
    );
    await page.locator("#send").click();
    expect((await send).ok()).toBe(true);
    await expect(row).toHaveCount(0);
    await expect(page.locator("#composer .execution-menu")).toHaveCount(1);
  } finally {
    fixture.kill("SIGTERM");
  }
});
