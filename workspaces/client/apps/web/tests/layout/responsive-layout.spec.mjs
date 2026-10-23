import { test, expect } from "../playwright.mjs";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.mjs";
import { detectTextClipping } from "./text-clipping.mjs";

for (const paired of [false, true]) {
  test(`conversation controls fit the measured column and large text (${paired ? "combined" : "direct"})`, async ({
    page,
    context,
  }) => {
    await page.setViewportSize({ width: 1200, height: 900 });
    const fixture = await sidebarParityFixture(context, { paired });
    try {
      const sidebar = await fixture.open(page);
      await sidebar
        .getByRole("button", { name: /^Local Saved pin\b/ })
        .first()
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
        expect(overflow, `document overflow at ${width}`).toBeLessThanOrEqual(
          1,
        );
      }
      await page.setViewportSize({ width: 1600, height: 900 });
      await page.locator(".workspace-layout-row").evaluate((row) => {
        row.style.flex = "0 0 700.5px";
      });
      await expect(
        page.locator(".conversation-header-tools-menu"),
      ).toHaveAttribute("data-compact", "yes");
      await page.locator(".workspace-layout-row").evaluate((row) => {
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
      await page.screenshot({ path: test.info().outputPath("layout.png") });
    } finally {
      await fixture.close();
    }
  });
}
