import { fixtureOrigin } from "../../tests/misc-fixture.ts";
import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../tests/playwright.ts";

// The phone "Servers and chats" bar shows only where no edge rail exists.
// The rail's sidebar button opens servers and chats on the iPhone Duo.
const cases = [
  {
    name: "ordinary phone portrait",
    width: 390,
    height: 844,
    rail: "",
    drawerTop: 0,
  },
  {
    name: "Duo portrait with a top rail",
    width: 669,
    height: 951,
    rail: "top&strip",
    drawerTop: 82,
  },
  {
    name: "Duo open landscape",
    width: 951,
    height: 669,
    rail: "right",
    drawerTop: 0,
  },
  { name: "Duo closed", width: 402, height: 874, rail: "right", drawerTop: 0 },
];

test("the phone servers bar hides whenever the edge rail shows", async ({
  browser,
}) => {
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "server-bar-fixture",
        configureServer(viteServer) {
          viteServer.middlewares.use((request, response, next) => {
            if (!request.url?.startsWith("/bar-check")) return next();
            response.setHeader("Content-Type", "text/html");
            response.end(`<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
            <title>Servers bar fixture</title><div id="root"></div><script type="module" src="/src/servers/serverBarRail.fixture.ts"></script>`);
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    for (const entry of cases) {
      const page = await browser.newPage({
        viewport: { width: entry.width, height: entry.height },
        hasTouch: true,
        isMobile: true,
      });
      await page.goto(
        `${fixtureOrigin(server)}/bar-check${entry.rail ? `?rail=${entry.rail}` : ""}`,
      );
      await expect(page.locator("body[data-ready]")).toHaveCount(1);
      const bar = page.locator(".server-mobile-toggle");
      const padding = await page
        .locator(".multi-server-shell")
        .evaluate((node) => getComputedStyle(node).paddingTop);
      // Drawers start below the native portrait top strip and its rail row.
      const drawerTop = await page
        .locator(".mantine-Drawer-inner")
        .evaluate((node) => node.getBoundingClientRect().top);
      expect(drawerTop, entry.name).toBe(entry.drawerTop);
      if (entry.rail) {
        await expect(bar, entry.name).toBeHidden();
        expect(padding, entry.name).toBe("0px");
      } else {
        await expect(bar, entry.name).toBeVisible();
        expect(padding, entry.name).toBe("40px");
      }
      await page.close();
    }
  } finally {
    await server.close();
  }
});
