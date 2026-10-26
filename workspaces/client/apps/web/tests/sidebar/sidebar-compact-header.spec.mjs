import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

for (const width of [1440, 390, 320]) {
  for (const colorScheme of ["light", "dark"]) {
    test(`compact sidebar header ${width} ${colorScheme}`, async ({
      page,
    }, testInfo) => {
      await page.setViewportSize({ width, height: 900 });
      await page.emulateMedia({ colorScheme });
      await page.addInitScript(
        (scheme) => localStorage.setItem("mantine-color-scheme-value", scheme),
        colorScheme,
      );
      const server = await fixture("Local");
      try {
        await page.goto(server.origin);
        const sidebar = page.locator("#sidebar");
        await expect(sidebar).toBeVisible();
        await expect(
          sidebar.locator(".sidebar-nav, .sidebar-primary-actions, kbd"),
        ).toHaveCount(0);
        await expect(
          sidebar.getByRole("button", { name: /^Studio errors/ }),
        ).toHaveCount(0);
        const header = sidebar.locator(".sidebar-header");
        const search = header.getByRole("button", {
          name: "Search chats",
          exact: true,
        });
        const settings = header.getByRole("button", {
          name: "Studio settings",
        });
        await expect(search).toHaveAttribute(
          "aria-keyshortcuts",
          "Control+k Meta+k",
        );
        await expect(search).toHaveAttribute("title", /Ctrl\+K/);
        await expect(search).toHaveText("");
        const projects = sidebar.locator(".projects-heading");
        const newChat = projects.getByRole("button", {
          name: "New chat",
          exact: true,
        });
        const newShared = projects.getByRole("button", {
          name: "New shared chat",
          exact: true,
        });
        const addProject = projects.getByRole("button", {
          name: "Add project",
          exact: true,
        });
        for (const control of [
          search,
          settings,
          newChat,
          newShared,
          addProject,
        ]) {
          await expect(control).toBeInViewport({ ratio: 1 });
          const bounds = await control.boundingBox();
          if (width < 760) {
            const target = await control.evaluate(
              () =>
                parseFloat(
                  getComputedStyle(document.documentElement).getPropertyValue(
                    "--studio-touch-target",
                  ),
                ) *
                parseFloat(getComputedStyle(document.documentElement).fontSize),
            );
            expect(bounds.width).toBeGreaterThanOrEqual(target);
            expect(bounds.height).toBeGreaterThanOrEqual(target);
          }
        }
        await search.focus();
        await page.keyboard.press("Tab");
        await expect(settings).toBeFocused();
        await newChat.focus();
        await page.keyboard.press("Tab");
        await expect(newShared).toBeFocused();
        await page.keyboard.press("Tab");
        await expect(addProject).toBeFocused();
        await expect(
          sidebar.getByRole("searchbox", { name: "Filter projects and chats" }),
        ).toBeVisible();
        await page.screenshot({
          path: testInfo.outputPath(`sidebar-${width}-${colorScheme}.png`),
          animations: "disabled",
        });
        await search.click();
        await expect(
          page.getByRole("dialog", { name: "Search messages" }),
        ).toBeVisible();
        await page
          .getByRole("dialog", { name: "Search messages" })
          .getByRole("button", { name: "Close", exact: true })
          .click();
        await page.keyboard.press("Control+k");
        await expect(
          page.getByRole("dialog", { name: "Search messages" }),
        ).toBeVisible();
        await page
          .getByRole("dialog", { name: "Search messages" })
          .getByRole("button", { name: "Close", exact: true })
          .click();
        for (const [button, title] of [
          [newChat, "New chat"],
          [newShared, "New shared chat"],
        ]) {
          if (width < 760) {
            await page
              .getByRole("button", { name: "Back to chats", exact: true })
              .click();
            await expect(sidebar).toBeInViewport({ ratio: 1 });
          }
          await button.click();
          const dialog = page.getByRole("dialog", { name: title, exact: true });
          await expect(dialog).toBeVisible();
          await dialog
            .getByRole("button", { name: "Close", exact: true })
            .click();
        }
      } finally {
        await server.close();
      }
    });
  }
}
