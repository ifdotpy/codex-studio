import { test, expect, spawnFixture, rustUiFixture } from "../playwright.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("desktop conversation header keeps title and actions on one row", async ({
  page,
}) => {
  test.setTimeout(90_000);
  const state = await mkdtemp(join(tmpdir(), "studio-conversation-chrome-"));
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["ignore", "pipe", "pipe"],
  });
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
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .first()
      .click();
    await expect(page.locator("#conversation-title")).toHaveText(
      "Other project",
    );
    await expect(
      page.locator("#conversation-header-tools .prompt-navigation-compact"),
    ).toBeVisible();
    const header = page.locator(".simple-workspace-header");
    const geometry = await header.evaluate((element) => {
      const rect = (selector) =>
        element.querySelector(selector).getBoundingClientRect().toJSON();
      return [
        "#sidebar-toggle",
        ".conversation-heading",
        "#conversation-header-tools",
        "#chat-settings-toggle",
        "#messages-toggle",
        '[aria-label="Chat actions"]',
      ].map((selector) => ({ selector, ...rect(selector) }));
    });
    const centers = geometry.map((item) => item.y + item.height / 2);
    expect(
      Math.max(...centers) - Math.min(...centers),
      JSON.stringify(geometry),
    ).toBeLessThanOrEqual(1);
    for (let index = 1; index < geometry.length; index++) {
      expect(
        geometry[index].x,
        JSON.stringify(geometry),
      ).toBeGreaterThanOrEqual(geometry[index - 1].right);
    }
    await expect(page.locator("#conversation-status")).toHaveCount(0);
    const context = page.getByRole("button", {
      name: "Chat context",
      exact: true,
    });
    await expect(context).toHaveText("Context");
    await expect(context.getByRole("progressbar")).toHaveCount(0);
    await page.screenshot({
      path: test.info().outputPath("real-header-1440.png"),
    });
  } finally {
    fixture.kill("SIGTERM");
  }
});
