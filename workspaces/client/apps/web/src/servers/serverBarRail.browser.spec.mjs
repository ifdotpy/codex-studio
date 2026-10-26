import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../tests/playwright.mjs";

// The phone "Servers and chats" bar shows only where no edge rail exists.
// The rail's sidebar button opens servers and chats on the iPhone Duo.
const cases = [
  { name: "ordinary phone portrait", width: 390, height: 844, rail: false },
  {
    name: "Duo portrait with a top rail",
    width: 669,
    height: 951,
    rail: "top",
  },
  { name: "Duo open landscape", width: 951, height: 669, rail: "right" },
  { name: "Duo closed", width: 402, height: 874, rail: "right" },
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
        resolveId(id) {
          if (id === "/bar-entry") return "\0bar-entry";
        },
        load(id) {
          if (id !== "\0bar-entry") return;
          return `
          import '/src/layout/layout-contract.generated.css';
          import '/src/hooks/viewport-surface.css';
          import '/src/servers/servers.css';
          const side = new URLSearchParams(location.search).get('rail');
          document.querySelector('#root').innerHTML =
            '<div class="multi-server-shell" data-navigation-compact="true">' +
            '<button class="server-mobile-toggle">Servers and chats</button>' +
            '<div class="server-views"><main>Conversation</main></div></div>';
          if (side) {
            const rail = document.createElement('nav');
            rail.className = 'edge-rail';
            rail.dataset.side = side;
            document.body.append(rail);
          }
          document.body.dataset.ready = 'true';
        `;
        },
        configureServer(viteServer) {
          viteServer.middlewares.use((request, response, next) => {
            if (!request.url?.startsWith("/bar-check")) return next();
            response.setHeader("Content-Type", "text/html");
            response.end(`<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
            <title>Servers bar fixture</title><div id="root"></div><script type="module" src="/bar-entry"></script>`);
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const port = server.httpServer.address().port;
    for (const entry of cases) {
      const page = await browser.newPage({
        viewport: { width: entry.width, height: entry.height },
        hasTouch: true,
        isMobile: true,
      });
      await page.goto(
        `http://127.0.0.1:${port}/bar-check${entry.rail ? `?rail=${entry.rail}` : ""}`,
      );
      await expect(page.locator("body[data-ready]")).toHaveCount(1);
      const bar = page.locator(".server-mobile-toggle");
      const padding = await page
        .locator(".multi-server-shell")
        .evaluate((node) => getComputedStyle(node).paddingTop);
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
