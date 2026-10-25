import { test, expect } from "../playwright.mjs";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.mjs";
import { detectTextClipping } from "./text-clipping.mjs";

test.use({ hasTouch: true });

for (const paired of [false, true]) {
  test(`conversation controls fit the measured column and large text (${paired ? "combined" : "direct"})`, async ({
    page,
    context,
    browserName,
  }) => {
    test.skip(
      browserName !== "chromium",
      "Safe-area fixtures use Chromium emulation",
    );
    const cdp = await context.newCDPSession(page);
    await page.setViewportSize({ width: 1200, height: 900 });
    const fixture = await sidebarParityFixture(context, { paired });
    try {
      const sidebar = await fixture.open(page);
      await sidebar
        .getByRole("button", { name: /^Local Saved pin\b/ })
        .first()
        .click();
      await expect(page.locator("#message")).toBeVisible();
      for (const width of [
        320, 360, 390, 430, 760, 761, 951, 1199, 1200, 1440,
      ]) {
        await cdp.send("Emulation.setSafeAreaInsetsOverride", {
          insets: {
            top: 0,
            right: width === 951 ? 84 : 0,
            bottom: width === 951 ? 34 : 0,
            left: 0,
          },
        });
        await page.setViewportSize({ width, height: 669 });
        await expect(
          page.getByRole("button", { name: "Chat actions", exact: true }),
        ).toBeVisible();
        if (width === 951) {
          await expect(
            page.locator('#sidebar[data-presentation="inline"]'),
          ).toBeVisible();
          const rail = page.getByRole("navigation", {
            name: "Conversation actions",
          });
          await expect(rail).toBeVisible();
          await expect
            .poll(() =>
              page
                .locator(".workspace")
                .evaluate((node) => node.getBoundingClientRect().right),
            )
            .toBeLessThanOrEqual(867);
          const bounds = await rail.boundingBox();
          expect(bounds.x).toBe(867);
          expect(bounds.y).toBeGreaterThanOrEqual(112);
          expect(bounds.y + bounds.height).toBeLessThanOrEqual(635);
          const band = page.locator(".viewport-bottom-band");
          await expect(band).toHaveCSS("height", "34px");
          await expect(page.locator(".usage-passive-band")).toBeVisible();
          const bandBounds = await band.boundingBox();
          expect(bandBounds.y).toBe(635);
          expect(bandBounds.y + bandBounds.height).toBe(669);
          for (const target of await page
            .locator("#usage-footer button")
            .all()) {
            const box = await target.boundingBox();
            if (box) expect(box.y + box.height).toBeLessThanOrEqual(635);
          }
          console.log("Duo bottom band", {
            paired,
            oldEmptyBand: 34,
            newEmptyBand: 0,
            passive: await page.locator(".usage-passive-band").boundingBox(),
          });
          const composer = await page.locator("#composer").boundingBox();
          expect(composer.y + composer.height).toBeLessThanOrEqual(635);
          await page.screenshot({
            path: test.info().outputPath("duo-safe-layout.png"),
          });
        }
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
