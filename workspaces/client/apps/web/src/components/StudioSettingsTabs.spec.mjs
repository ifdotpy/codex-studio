import { test, expect } from "../../tests/playwright.mjs";
import { fixture } from "../../tests/servers/multi-server-fixture.mjs";

for (const width of [320, 390]) {
  test(`Studio settings at ${width} keeps navigation separate from the form`, async ({
    page,
  }) => {
    const local = await fixture("Settings phone");
    try {
      await page.goto(local.origin);
      await page.locator("#message").waitFor();
      await page
        .getByRole("button", { name: "Studio settings", exact: true })
        .click();
      await page.setViewportSize({ width, height: 568 });
      const settings = page.getByRole("dialog", {
        name: "Studio settings",
        exact: true,
      });
      const appearance = settings.getByRole("tab", {
        name: "Appearance",
        exact: true,
      });
      await expect(appearance).toBeVisible();
      await expect(settings.locator(".studio-settings-content")).toBeHidden();
      await appearance.click();
      const heading = settings
        .locator(".studio-settings-detail-heading")
        .getByRole("heading", { name: "Appearance" });
      await expect(heading).toBeFocused();
      const oldChats = settings.getByRole("textbox", {
        name: "Hide old chats",
        exact: true,
      });
      await oldChats.fill("32");
      await heading.click();
      await settings.getByRole("button", { name: "Back", exact: true }).click();
      await expect(appearance).toBeFocused();
      await appearance.click();
      await expect(oldChats).toHaveValue("32");
      expect(
        await settings.evaluate(
          (element) => element.scrollWidth <= element.clientWidth,
        ),
      ).toBe(true);
      expect(
        await settings
          .locator(".studio-settings-content")
          .evaluate((element) => getComputedStyle(element).overflowY),
      ).toBe("auto");
      expect(
        await settings
          .getByRole("tabpanel", { name: "Appearance", exact: true })
          .evaluate((element) => getComputedStyle(element).overflowY),
      ).toBe("visible");
    } finally {
      await local.close();
    }
  });
}
