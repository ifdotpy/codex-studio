import { fixtureOrigin } from "../../tests/misc-fixture.ts";
import { visibleBounds } from "../../tests/mobile/mobile-ui-fixture.ts";
import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../tests/playwright.ts";

test("Duo uses the bottom band for passive status and keeps every target above Home", async ({
  browser,
}) => {
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "bottom-band-fixture",
        configureServer(s) {
          s.middlewares.use((req, res, next) => {
            if (req.url?.split("?")[0] !== "/bottom-check") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover"><div id="root"></div><script type="module" src="/src/hooks/bottomInset.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  const page = await browser.newPage({
    viewport: { width: 951, height: 669 },
    hasTouch: false,
    reducedMotion: "reduce",
  });
  await page.addInitScript(() =>
    Object.defineProperty(navigator, "maxTouchPoints", { value: 5 }),
  );
  try {
    await server.listen();
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 84, bottom: 34, left: 0 },
    });
    await page.goto(`${fixtureOrigin(server)}/bottom-check`);
    expect(
      await page.evaluate(() => matchMedia("(any-pointer: coarse)").matches),
    ).toBe(false);
    await expect(page.locator("#message")).toHaveCSS("font-size", "16px");
    expect(
      await page.locator("#message").evaluate((n) => {
        const event = new PointerEvent("pointerdown", {
          bubbles: true,
          cancelable: true,
          pointerType: "touch",
        });
        n.dispatchEvent(event);
        return event.defaultPrevented;
      }),
    ).toBe(false);
    await expect(page.locator("html")).not.toHaveAttribute(
      "data-viewport-surface",
    );
    const status = page.locator(".session-cost-summary");
    await expect(status).toBeVisible();
    const before = await visibleBounds(status);
    console.log(
      "passive-status-before-check",
      JSON.stringify(before),
      "empty-band-height",
      34,
    );
    await expect
      .poll(async () => (await visibleBounds(status)).y)
      .toBeGreaterThanOrEqual(635);
    const rect = await visibleBounds(status);
    expect(rect.y + rect.height).toBeLessThanOrEqual(669);
    await expect(page.locator(".token-rate")).toBeVisible();
    for (const target of await page.locator("button,textarea").all()) {
      const box = await visibleBounds(target);
      expect(box.y + box.height).toBeLessThanOrEqual(635);
    }
    const band = page.locator(".viewport-bottom-band");
    await expect(band).toHaveCSS("height", "34px");
    await expect(band).toHaveCSS("pointer-events", "none");
    expect(
      await band.locator("button,a,input,textarea,[tabindex]").count(),
    ).toBe(0);
    await page.screenshot({ path: test.info().outputPath("bottom-band.png") });
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 0, bottom: 0, left: 0 },
    });
    await expect(band).toHaveCount(0);
    await expect(status).toBeVisible();
  } finally {
    await page.close();
    await server.close();
  }
});
