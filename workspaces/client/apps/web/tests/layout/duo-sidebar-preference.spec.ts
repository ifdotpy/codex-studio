import { duoPhoneUserAgent } from "./duo-fixture.ts";
import { test, expect } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";

test.use({
  userAgent: duoPhoneUserAgent,
  hasTouch: true,
  reducedMotion: "reduce",
  serviceWorkers: "block",
});

test("Duo has its own sidebar preference and keeps one header row", async ({
  page,
  context,
}) => {
  const fixture = await sidebarParityFixture(context, { paired: false });
  await page.setViewportSize({ width: 1200, height: 669 });
  try {
    await fixture.open(page);
    const resize = page.getByRole("separator", {
      name: "Conversation sidebar width",
    });
    await resize.press("ArrowLeft");
    const desktopKey = await page.evaluate(() => {
      const key = Object.keys(localStorage).find((key) =>
        key.startsWith("studio-sidebar-width:"),
      );
      if (!key) throw new Error("The sidebar did not save its width");
      localStorage.setItem(key, "229");
      return key;
    });
    await page.reload();
    const width = async () =>
      (await page.locator("#sidebar").boundingBox())?.width;
    await expect.poll(width).toBeCloseTo(229, 1);
    await page.setViewportSize({ width: 951, height: 669 });
    await page.screenshot({
      path: test.info().outputPath("duo-saved-width.png"),
    });
    await expect.poll(width).toBeCloseTo(316.8, 1);
    const rows = await page.locator(".sidebar-header").evaluate((header) => {
      const centers = Array.from(header.children)
        .map((node) => node.getBoundingClientRect())
        .filter((rect) => rect.width && rect.height)
        .map((rect) => Math.round(rect.y + rect.height / 2));
      return new Set(centers).size;
    });
    expect(rows).toBe(1);
    await resize.press("ArrowLeft");
    await expect.poll(width).toBeCloseTo(300.8, 1);
    await page.setViewportSize({ width: 1200, height: 669 });
    await expect.poll(width).toBeCloseTo(229, 1);
    expect(
      await page.evaluate((key) => localStorage.getItem(key), desktopKey),
    ).toBe("229");
    await page.setViewportSize({ width: 951, height: 669 });
    await expect.poll(width).toBeCloseTo(300.8, 1);
  } finally {
    await fixture.close();
  }
});
