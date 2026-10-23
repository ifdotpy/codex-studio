import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  stubSessionCost,
} from "../playwright.mjs";
import { detectTextClipping } from "./text-clipping.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("conversation controls fit the measured column and large text", async ({
  page,
}) => {
  test.setTimeout(90_000);
  const root = await mkdtemp(join(tmpdir(), "studio-responsive-layout-"));
  const fixture = spawnFixture(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
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
    await page.setViewportSize({ width: 1200, height: 900 });
    await stubSessionCost(page);
    await page.goto(
      `http://127.0.0.1:${port}/?studio-server=local&studio-navigation=classic`,
    );
    await page
      .locator("#sidebar [data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await expect(page.locator("#message")).toBeVisible();
    for (const width of [320, 360, 390, 430, 760, 761, 951, 1199, 1200]) {
      await page.setViewportSize({ width, height: 669 });
      await expect(
        page.getByRole("button", { name: "Chat actions", exact: true }),
      ).toBeVisible();
      const overflow = await page.evaluate(
        () =>
          document.documentElement.scrollWidth -
          document.documentElement.clientWidth,
      );
      expect(overflow, `document overflow at ${width}`).toBeLessThanOrEqual(1);
    }
    await page.setViewportSize({ width: 1600, height: 900 });
    await page.locator(".workspace-row").evaluate((row) => {
      row.style.flex = "0 0 700.5px";
    });
    await expect(
      page.locator(".conversation-header-tools-menu"),
    ).toHaveAttribute("data-compact", "yes");
    await page.locator(".workspace-row").evaluate((row) => {
      row.style.flex = "1 1 0";
    });
    await page.evaluate(() => {
      document.documentElement.style.setProperty(
        "--studio-main-font-size",
        "20px",
      );
    });
    await expect(
      page.locator(".conversation-header-tools-menu"),
    ).toHaveAttribute("data-compact", "yes");
    await page.locator("#conversation-title").evaluate((title) => {
      title.textContent =
        "A long conversation title with several words for the large text check";
      title.title = title.textContent;
    });
    const findings = await page.evaluate(detectTextClipping, {
      scene: "large text header",
      profile: "20px",
    });
    expect(
      findings.filter(
        (row) =>
          !row.intentional &&
          row.selector.includes("conversation-header-tools"),
      ),
      JSON.stringify(findings),
    ).toEqual([]);
    await page.screenshot({ path: "/tmp/responsive-layout-slice1.png" });
  } finally {
    fixture.kill("SIGTERM");
  }
});
