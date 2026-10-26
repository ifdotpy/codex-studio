import { test, expect } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";
test.use({ hasTouch: true, reducedMotion: "reduce", serviceWorkers: "block" });
for (const size of [
  { width: 951, height: 669 },
  { width: 653, height: 951 },
  { width: 669, height: 951 },
]) {
  test(`Duo ${size.width}x${size.height} has one header row and one footer control`, async ({
    page,
    context,
  }) => {
    const fixture = await sidebarParityFixture(context, { paired: false });
    await page.setViewportSize({ width: 1200, height: 669 });
    try {
      await fixture.open(page);
      await page.setViewportSize(size);
      const sidebar = page.locator("#sidebar");
      if (!(await sidebar.isVisible()))
        await page.locator("#sidebar-toggle").click();
      await expect(sidebar).toBeVisible();
      const header = sidebar.locator(".sidebar-header");
      await expect(header.getByRole("button")).toHaveCount(1);
      const search = header.getByRole("button", { name: "Search chats" });
      await expect(search).toBeVisible();
      const brand = (await header.locator(".brand").boundingBox())!;
      const button = (await search.boundingBox())!;
      expect(brand.y + brand.height / 2).toBeCloseTo(
        button.y + button.height / 2,
        0,
      );
      expect(button.width).toBe(48);
      await expect(
        sidebar.getByRole("button", { name: "Close conversations" }),
      ).toHaveCount(0);
      const footer = sidebar.locator(".sidebar-utility-footer");
      await expect(footer.getByRole("button")).toHaveCount(1);
      await footer
        .getByRole("button", { name: "Sidebar settings and warnings" })
        .click();
      await expect(
        page.getByRole("menuitem", { name: "Studio settings", exact: true }),
      ).toBeVisible();
      await page.getByRole("menuitem", { name: /^Warnings/ }).click();
      await expect(
        page.getByRole("dialog", { name: "Warnings", exact: true }),
      ).toBeVisible();
      await page.keyboard.press("Escape");
      await page.screenshot({
        path: test.info().outputPath("sidebar-header.png"),
      });
    } finally {
      await fixture.close();
    }
  });
}
