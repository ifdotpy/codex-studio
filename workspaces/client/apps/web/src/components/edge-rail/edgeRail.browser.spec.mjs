import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../../tests/playwright.mjs";

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
          resolveId(id) {
            if (id === "/rail-entry") return "\0rail-entry";
          },
          load(id) {
            if (id !== "\0rail-entry") return;
            return `
          import React from 'react';
          import {createRoot} from 'react-dom/client';
          import {MantineProvider,Menu} from '@mantine/core';
          import {EdgeRail} from '/src/components/edge-rail/EdgeRail.tsx';
          import {useEdgeRail} from '/src/components/edge-rail/useEdgeRail.ts';
          import {theme} from '/src/theme.ts';
          import '@mantine/core/styles.css';
          import '/src/layout/layout-contract.generated.css';
          const e=React.createElement;
          function Fixture(){
            const geometry=useEdgeRail();
            const [last,setLast]=React.useState('None');
            return e(MantineProvider,{theme},
              e('main',null,e('h1',null,'Conversation title'),e('output',{id:'last-action'},last)),
              e(EdgeRail,{geometry,
                sidebar:{expanded:true,onClick:()=>setLast('Toggle conversations')},
                messages:{count:3,onClick:()=>setLast('Messages')},
                menu:e(Menu.Item,{onClick:()=>setLast('Existing action')},'Existing action')}));
          }
          createRoot(document.querySelector('#root')).render(e(Fixture));
        `;
          },
          configureServer(server) {
            server.middlewares.use((request, response, next) => {
              if (request.url !== "/rail-check") return next();
              response.setHeader("Content-Type", "text/html");
              response.end(`<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
            <title>Edge rail fixture</title><style>*{box-sizing:border-box}body{margin:0}#root{display:flex;height:100dvh;padding:0 84px 34px 0}main{flex:1;min-width:0;height:100%;background:#eee}</style>
            <div id="root"></div><script type="module" src="/rail-entry"></script>`);
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
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    try {
      await server.listen();
      await page.goto(
        `http://127.0.0.1:${server.httpServer.address().port}/rail-check`,
      );
      const rail = page.getByRole("navigation", {
        name: "Conversation actions",
      });
      await expect(rail).toBeVisible();
      expect(await rail.boundingBox()).toEqual({
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
        const rect = await button.boundingBox();
        expect(rect.width).toBeGreaterThanOrEqual(44);
        expect(rect.height).toBeGreaterThanOrEqual(44);
        expect(rect.y + rect.height).toBeLessThanOrEqual(635);
        await button.click();
        await expect(page.locator("#last-action")).toHaveText(
          label.startsWith("Messages") ? "Messages" : label,
        );
      }
      await rail
        .getByRole("button", { name: "Chat actions", exact: true })
        .click();
      const item = page.getByRole("menuitem", { name: "Existing action" });
      await expect(item).toBeVisible();
      expect((await item.boundingBox()).x).toBeLessThan(867);
      await item.click();
      await expect(page.locator("#last-action")).toHaveText("Existing action");

      await page.evaluate(() => {
        const root = document.querySelector("#root");
        root.style.paddingRight = "0px";
        root.style.paddingLeft = "84px";
      });
      await expect(rail).toHaveAttribute("data-side", "left");
      expect((await rail.boundingBox()).x).toBe(0);
      await rail
        .getByRole("button", { name: "Chat actions", exact: true })
        .click();
      await expect(item).toBeVisible();
      expect((await item.boundingBox()).x).toBeGreaterThanOrEqual(44);
      await expect(page.getByRole("menu")).toHaveCSS("opacity", "1");
      await page.screenshot({
        path: test.info().outputPath("edge-rail-left-menu.png"),
        animations: "disabled",
      });
      await page.keyboard.press("Escape");
      await expect(item).toHaveCount(0);

      await page.evaluate(() => {
        const root = document.querySelector("#root");
        root.style.height = "240px";
        root.style.paddingBottom = "0px";
      });
      await expect
        .poll(async () => (await rail.boundingBox()).height)
        .toBe(128);
      await rail
        .getByRole("button", { name: "Chat actions", exact: true })
        .focus();
      await page.keyboard.press("Enter");
      await expect(item).toBeVisible();
      await expect(
        page.locator("[data-menu-dropdown] [data-autofocus]"),
      ).toBeFocused();
      await page.keyboard.press("ArrowDown");
      await expect(item).toBeFocused();
      await page.keyboard.press("Escape");
      await expect(item).toHaveCount(0);
      expect(
        await rail.evaluate((node) => node.scrollHeight > node.clientHeight),
      ).toBe(true);
      await page.evaluate(() => {
        document.querySelector("#root").style.paddingLeft = "43.99px";
      });
      await expect(rail).toHaveCount(0);
      expect(errors).toEqual([]);
    } finally {
      await page.close();
      await server.close();
    }
  });
}
