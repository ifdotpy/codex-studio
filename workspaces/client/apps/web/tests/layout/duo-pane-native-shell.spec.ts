import { test, expect } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";

test.use({ hasTouch: true, serviceWorkers: "block", reducedMotion: "reduce" });

for (const size of [
  { name: "open", width: 951, height: 669, hinge: "open" },
  { name: "portrait", width: 669, height: 951, hinge: "open" },
  { name: "closed", width: 653, height: 951, hinge: "closed" },
]) {
  test(`The paired-server ${size.name} pane follows the native top rail`, async ({
    page,
    context,
  }) => {
    const top = size.height > size.width ? 82 : 0;
    const fixture = await sidebarParityFixture(context);
    fixture.local.snapshot.threads.push(
      ...Array.from({ length: 3 }, (_, index) => ({
        ...fixture.local.snapshot.threads[0]!,
        id: `native-worker-${index}`,
        rootId: "overlap",
        isLead: false,
        name: `Worker ${index}`,
      })),
    );
    await page.route(
      (url) => url.origin === fixture.local.origin && url.pathname === "/",
      async (route) => {
        const response = await route.fetch();
        const headers = response.headers();
        headers["content-security-policy"] = headers[
          "content-security-policy"
        ].replace("script-src 'self'", "script-src 'self' 'wasm-unsafe-eval'");
        await route.fulfill({ response, headers });
      },
    );
    const cdp = await context.newCDPSession(page);
    try {
      await page.setViewportSize({ width: 1200, height: 951 });
      const sidebar = await fixture.open(page);
      await sidebar
        .getByRole("button")
        .filter({ hasText: "Local Saved pin" })
        .filter({ has: page.locator("strong") })
        .first()
        .click();
      await cdp.send("Emulation.setSafeAreaInsetsOverride", {
        insets: { top: 0, right: 84, bottom: 34, left: 0 },
      });
      await page.setViewportSize({ width: size.width, height: size.height });
      // Stand-in for the native top-strip contract until #528 merges.
      await page.locator(".edge-rail").evaluate(
        (rail, { hinge, top }) => {
          const root = document.documentElement;
          root.dataset.nativeHinge = hinge;
          if (top) root.setAttribute("data-viewport-top-strip", "");
          root.style.setProperty("--native-safe-top", `${top}px`);
          root.style.setProperty("--viewport-overlay-inset-top", `${top}px`);
          if (top) rail.setAttribute("data-side", "top");
          if (top)
            Object.assign((rail as HTMLElement).style, {
              position: "fixed",
              left: "0px",
              right: "auto",
              top: "0px",
              width: "529px",
              height: "82px",
              flexDirection: "row",
              justifyContent: "flex-end",
              alignItems: "flex-end",
              paddingBottom: "10px",
              gap: "8px",
            });
          window.dispatchEvent(new Event("studio-native-viewport"));
        },
        { hinge: size.hinge, top },
      );
      const opener = page.locator(".edge-rail #sidebar-toggle");
      if (await sidebar.isVisible()) await opener.click();
      await expect(sidebar).toBeHidden();
      await opener.click();
      await expect(sidebar).toBeVisible();
      await expect(opener).toHaveAttribute("aria-expanded", "true");
      const side = (await sidebar.boundingBox())!;
      expect(side.y).toBe(top);
      expect(side.y + side.height).toBe(size.height);
      await page.screenshot({
        path: test.info().outputPath(`${size.name}-native-sidebar.png`),
      });
      await opener.click();
      await page
        .getByRole("button", { name: "Chat actions", exact: true })
        .click();
      await page.getByRole("menuitem", { name: "Team", exact: true }).click();
      const panel = page.locator(
        '.workspace-panel-inline.duo-pane-panel[aria-label="Team panel"]',
      );
      await expect(panel).toBeVisible();
      await expect(sidebar).toBeHidden();
      const box = (await panel.boundingBox())!;
      expect(box.y).toBe(top);
      expect(box.y + box.height).toBe(size.height);
      expect(
        (await panel.locator(".duo-pane-header").boundingBox())!.y,
      ).toBeGreaterThanOrEqual(top);
      if (!top) {
        const conversation = (await page.locator(".workspace").boundingBox())!;
        expect(box.x).toBeCloseTo(conversation.x + conversation.width, 1);
        expect(box.width).toBeCloseTo(conversation.width, 1);
      }
      await page.screenshot({
        path: test.info().outputPath(`${size.name}-native-team.png`),
      });
    } finally {
      await cdp.detach().catch(() => {});
      await fixture.close();
    }
  });
}

test("A paired Duo keeps the Terminals footer on its first open", async ({
  page,
  context,
}) => {
  const fixture = await sidebarParityFixture(context);
  const cdp = await context.newCDPSession(page);
  try {
    await page.setViewportSize({ width: 951, height: 669 });
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 84, bottom: 34, left: 0 },
    });
    const sidebar = await fixture.open(page);
    await sidebar
      .getByRole("button")
      .filter({ hasText: "Local Saved pin" })
      .filter({ has: page.locator("strong") })
      .first()
      .click();
    const footer = page.locator(".shell-terminal-bar");
    await expect(footer).toBeVisible();
    await footer
      .getByRole("button", { name: "Show terminals", exact: true })
      .click();
    await expect(page.locator(".terminal-pane-path")).toBeVisible();
    await expect(sidebar).toBeHidden();
  } finally {
    await cdp.detach().catch(() => {});
    await fixture.close();
  }
});
