import { duoPhoneUserAgent } from "./duo-fixture.ts";
import { test, expect } from "../playwright.ts";
import { sidebarParityFixture } from "../sidebar/sidebar-parity-fixture.ts";

const profiles = [
  { name: "iphone-portrait", width: 390, height: 844, right: 0, bottom: 34 },
  { name: "iphone-landscape", width: 844, height: 390, right: 59, bottom: 21 },
  { name: "duo-closed", width: 653, height: 951, right: 0, bottom: 34 },
  { name: "duo-portrait", width: 669, height: 951, right: 0, bottom: 34 },
  { name: "duo-open", width: 951, height: 669, right: 84, bottom: 34 },
];
test.use({
  userAgent: duoPhoneUserAgent,
  hasTouch: true,
  reducedMotion: "reduce",
  serviceWorkers: "block",
});
for (const profile of profiles) {
  for (const kind of ["sidebar", "team", "workspace"] as const) {
    test(`${profile.name} ${kind} drawer covers the Home band`, async ({
      page,
      context,
    }) => {
      test.skip(
        kind === "sidebar" && profile.name === "duo-open",
        "The open Duo sidebar is inline",
      );
      const fixture = await sidebarParityFixture(context, { paired: false });
      fixture.local.snapshot.threads.push({
        ...fixture.local.snapshot.threads[0]!,
        id: "drawer-worker",
        rootId: "overlap",
        isLead: false,
        name: "Drawer worker",
        status: "running",
        inFlight: true,
      });
      fixture.local.snapshot.runtime.requests = [];
      fixture.local.snapshot.runtime.complaints = [];
      const cdp = await context.newCDPSession(page);
      await page.setViewportSize({ width: 1200, height: profile.height });
      try {
        const sidebar = await fixture.open(page);
        await sidebar
          .getByRole("button")
          .filter({ hasText: "Local Saved pin" })
          .filter({ has: page.locator("strong") })
          .first()
          .click();
        await cdp.send("Emulation.setSafeAreaInsetsOverride", {
          insets: {
            top: 0,
            right: profile.right,
            bottom: profile.bottom,
            left: 0,
          },
        });
        await page.setViewportSize({
          width: profile.width,
          height: profile.height,
        });
        if (kind === "sidebar") {
          await page.locator("#sidebar-toggle").click();
        } else {
          await page
            .getByRole("button", { name: "Chat actions", exact: true })
            .click();
          await page
            .getByRole("menuitem", {
              name: kind === "team" ? "Team" : "Plan",
              exact: true,
            })
            .click();
        }
        const drawer = page.locator(".mantine-Drawer-content:visible");
        await expect(drawer).toBeVisible();
        await page.screenshot({ path: test.info().outputPath("drawer.png") });
        await expect
          .poll(async () => {
            const rect = (await drawer.boundingBox())!;
            return rect.y + rect.height;
          })
          .toBeCloseTo(profile.height, 1);
        const bottomHit = () =>
          drawer.evaluate((node) => {
            const rect = node.getBoundingClientRect();
            const hit = document.elementFromPoint(
              rect.x + rect.width / 2,
              window.innerHeight - 1,
            );
            return {
              covered: !!hit && node.contains(hit),
              target: hit?.outerHTML.slice(0, 500),
              rect: rect.toJSON(),
            };
          });
        await expect.poll(async () => (await bottomHit()).covered).toBe(true);
        expect(
          await page.evaluate(({ width, height, right }) => {
            return [1, (width - right) / 2, width - right - 1].every((x) => {
              const hit = document.elementFromPoint(x, height - 1);
              return !!hit?.closest(
                ".mantine-Drawer-content, .mantine-Drawer-overlay",
              );
            });
          }, profile),
        ).toBe(true);
        await expect(page.locator(".usage-passive-band")).toBeHidden();
      } finally {
        await cdp.detach().catch(() => {});
        await fixture.close();
      }
    });
  }
}
