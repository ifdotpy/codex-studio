import { fixtureOrigin } from "../../../tests/misc-fixture.ts";
import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../../tests/playwright.ts";

for (const capability of [
  { name: "primary touch", primary: true, any: true, points: 5 },
  {
    name: "primary fine with a coarse secondary pointer",
    primary: false,
    any: true,
    points: 0,
  },
  {
    name: "touch hardware without a coarse match",
    primary: false,
    any: false,
    points: 5,
  },
]) {
  test(`edge rail follows physical insets and preserves actions (${capability.name})`, async ({
    browser,
  }) => {
    const server = await createFixtureViteServer({
      configFile: false,
      root: fileURLToPath(new URL("../../..", import.meta.url)),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "edge-rail-fixture",
          configureServer(server) {
            server.middlewares.use((request, response, next) => {
              if (request.url !== "/rail-check") return next();
              response.setHeader("Content-Type", "text/html");
              response.end(`<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
            <title>Edge rail fixture</title><style>*{box-sizing:border-box}body{margin:0}#root{display:flex;height:100dvh;padding:0 84px 34px 0}main{flex:1;min-width:0;height:100%;background:#eee}</style>
            <div id="root"></div><script type="module" src="/src/components/edge-rail/edgeRail.fixture.tsx"></script>`);
            });
          },
        },
      ],
    });
    const page = await browser.newPage({
      viewport: { width: 951, height: 669 },
      hasTouch: true,
      isMobile: true,
    });
    await page.addInitScript((capability) => {
      const original = window.matchMedia.bind(window);
      window.matchMedia = (query) => {
        const result = original(query);
        if (query === "(pointer: coarse)" || query === "(any-pointer: coarse)")
          Object.defineProperty(result, "matches", {
            value:
              query === "(pointer: coarse)"
                ? capability.primary
                : capability.any,
          });
        return result;
      };
      Object.defineProperty(navigator, "maxTouchPoints", {
        value: capability.points,
      });
    }, capability);
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    try {
      await server.listen();
      await page.goto(`${fixtureOrigin(server)}/rail-check`);
      const rail = page.getByRole("navigation", {
        name: "Conversation actions",
      });
      await expect(rail).toBeVisible();
      expect((await rail.boundingBox())!).toEqual({
        x: 867,
        y: 112,
        width: 84,
        height: 523,
      });
      await page.screenshot({
        path: test.info().outputPath("edge-rail-right.png"),
        animations: "disabled",
      });
      for (const label of ["Toggle conversations", "Messages, 3 need you"]) {
        const button = rail.getByRole("button", { name: label, exact: true });
        const rect = (await button.boundingBox())!;
        expect(rect.width).toBeGreaterThanOrEqual(44);
        expect(rect.height).toBeGreaterThanOrEqual(44);
        expect(rect.y + rect.height).toBeLessThanOrEqual(635);
        await button.click();
        await expect(page.locator("#last-action")).toHaveText(
          label.startsWith("Messages") ? "Messages" : label,
        );
      }
      // Buttons center on the system status column (edgeRail.statusColumnCenter).
      const actions = rail.getByRole("button", {
        name: "Chat actions",
        exact: true,
      });
      const actionsRect = (await actions.boundingBox())!;
      expect(actionsRect.x + actionsRect.width / 2).toBe(951 - 48);
      await actions.click();
      const item = page.getByRole("menuitem", { name: "Existing action" });
      await expect(item).toBeVisible();
      expect((await item.boundingBox())!.x).toBeLessThan(867);
      // The menu opens at its button, keeps edgeRail.menuGap from the strip,
      // and ends above Home.
      const menuRect = (await page.getByRole("menu").boundingBox())!;
      expect(Math.abs(menuRect.y - actionsRect.y)).toBeLessThanOrEqual(1);
      expect(menuRect.x + menuRect.width).toBeLessThanOrEqual(867 - 8);
      expect(menuRect.y + menuRect.height).toBeLessThanOrEqual(635);
      await item.click();
      await expect(page.locator("#last-action")).toHaveText("Existing action");

      await page.evaluate(() => {
        const root =
          document.querySelector<HTMLElement>("#root") ??
          (() => {
            throw new Error("Missing fixture element");
          })();
        root.style.paddingRight = "0px";
        root.style.paddingLeft = "84px";
      });
      await expect(rail).toHaveAttribute("data-side", "left");
      expect((await rail.boundingBox())!.x).toBe(0);
      const leftActions = (await actions.boundingBox())!;
      expect(leftActions.x + leftActions.width / 2).toBe(48);
      await actions.click();
      await expect(item).toBeVisible();
      expect((await item.boundingBox())!.x).toBeGreaterThanOrEqual(44);
      expect(
        (await page.getByRole("menu").boundingBox())!.x,
      ).toBeGreaterThanOrEqual(84 + 8);
      await expect(page.getByRole("menu")).toHaveCSS("opacity", "1");
      await page.screenshot({
        path: test.info().outputPath("edge-rail-left-menu.png"),
        animations: "disabled",
      });
      await page.keyboard.press("Escape");
      await expect(item).toHaveCount(0);

      await page.evaluate(() => {
        const root =
          document.querySelector<HTMLElement>("#root") ??
          (() => {
            throw new Error("Missing fixture element");
          })();
        root.style.height = "240px";
        root.style.paddingBottom = "0px";
      });
      await expect
        .poll(async () => (await rail.boundingBox())!.height)
        .toBe(128);
      await rail
        .getByRole("button", { name: "Chat actions", exact: true })
        .focus();
      await page.keyboard.press("Enter");
      await expect(item).toBeVisible();
      await expect(item).toBeFocused();
      await page.keyboard.press("Escape");
      await expect(item).toHaveCount(0);
      expect(
        await rail.evaluate((node) => node.scrollHeight > node.clientHeight),
      ).toBe(true);
      await page.evaluate(() => {
        (
          document.querySelector<HTMLElement>("#root") ??
          (() => {
            throw new Error("Missing fixture element");
          })()
        ).style.paddingLeft = "43.99px";
      });
      await expect(rail).toHaveCount(0);
      expect(errors).toEqual([]);
    } finally {
      await page.close();
      await server.close();
    }
  });
}
