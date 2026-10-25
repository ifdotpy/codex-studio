import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../tests/playwright.mjs";

const sequences = {
  "shrink then pan": [
    { height: 323, innerHeight: 323 },
    { offsetTop: 346, pageTop: 346, scrollY: 346 },
  ],
  "pan then shrink": [
    { offsetTop: 346, pageTop: 346, scrollY: 346 },
    { height: 323, innerHeight: 323 },
  ],
  "document scroll before offset": [
    { height: 323, innerHeight: 323 },
    { pageTop: 346, scrollY: 346 },
    { offsetTop: 346 },
  ],
  "offset before document scroll": [
    { height: 323, innerHeight: 323 },
    { pageTop: 346, offsetTop: 346 },
    { scrollY: 346 },
  ],
  "zero offset": [{ height: 323, innerHeight: 323 }],
  "transformed root": [
    {
      height: 323,
      innerHeight: 323,
      offsetTop: 346,
      pageTop: 346,
      scrollY: 346,
    },
  ],
  "visual origin": [
    {
      height: 323,
      innerHeight: 323,
      offsetTop: 346,
      pageTop: 346,
      scrollY: 346,
    },
  ],
};
for (const [name, steps] of Object.entries(sequences))
  test(`keyboard frame stays pinned: ${name}`, async ({ browser }) => {
    const server = await createFixtureViteServer({
      configFile: false,
      root: fileURLToPath(new URL("../..", import.meta.url)),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "keyboard-sequence-fixture",
          resolveId(id) {
            if (id === "/sequence-entry") return "\0sequence-entry";
          },
          load(id) {
            if (id !== "\0sequence-entry") return;
            return `import React from 'react';import {createRoot} from 'react-dom/client';import {useMobileViewport} from '/src/hooks/mobileViewport.ts';import '/src/hooks/viewport-surface.css';import '/src/layout/layout-contract.generated.css';
 const e=React.createElement;function Fixture(){useMobileViewport(true);return e(React.Fragment,null,e('header',{style:{height:44,flex:'none'}},'Header'),e('main',{id:'conversation',style:{display:'flex',flex:1,minHeight:0,flexDirection:'column'}},e('div',{id:'messages',style:{flex:1,minHeight:0,overflow:'auto'}},'Transcript '.repeat(100)),e('form',{id:'composer',style:{height:120,flex:'none',margin:0}},e('textarea',{id:'message',defaultValue:'Draft'}))));}createRoot(document.querySelector('#root')).render(e(Fixture));`;
          },
          configureServer(s) {
            s.middlewares.use((req, res, next) => {
              if (req.url !== "/sequence-check") return next();
              res.setHeader("Content-Type", "text/html");
              res.end(
                '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover"><style>*{box-sizing:border-box}body{margin:0}#root{display:flex;flex-direction:column}#composer{background:white}textarea{font-size:16px}</style><div id="root"></div><script type="module" src="/sequence-entry"></script>',
              );
            });
          },
        },
      ],
    });
    const page = await browser.newPage({
      viewport: { width: 867, height: 669 },
      hasTouch: true,
    });
    try {
      await server.listen();
      await page.addInitScript(
        ({ visualOrigin }) => {
          const viewport = Object.assign(new EventTarget(), {
            width: 867,
            height: 669,
            offsetTop: 0,
            pageTop: 0,
            scale: 1,
          });
          let innerHeight = 669,
            scrollY = 0;
          Object.defineProperty(window, "visualViewport", {
            configurable: true,
            value: viewport,
          });
          Object.defineProperty(window, "innerHeight", {
            configurable: true,
            get: () => innerHeight,
          });
          Object.defineProperty(window, "scrollY", {
            configurable: true,
            get: () => scrollY,
          });
          if (visualOrigin) {
            const rect = Element.prototype.getBoundingClientRect;
            Element.prototype.getBoundingClientRect = function () {
              const r = rect.call(this);
              return new DOMRect(
                r.x,
                r.y - viewport.pageTop,
                r.width,
                r.height,
              );
            };
          }
          window.stepKeyboard = (patch) => {
            innerHeight = patch.innerHeight ?? innerHeight;
            scrollY = patch.scrollY ?? scrollY;
            Object.assign(viewport, patch);
            viewport.dispatchEvent(new Event("resize"));
            viewport.dispatchEvent(new Event("scroll"));
            window.dispatchEvent(new Event("scroll"));
            const root = document
                .querySelector("#root")
                .getBoundingClientRect(),
              composer = document
                .querySelector("#composer")
                .getBoundingClientRect();
            return {
              top: root.top,
              rootHeight: root.height,
              composerTop: composer.top,
              composerBottom: composer.bottom,
              visibleTop: visualOrigin ? 0 : viewport.pageTop,
              visibleHeight: viewport.height,
            };
          };
        },
        { visualOrigin: name === "visual origin" },
      );
      await page.goto(
        `http://127.0.0.1:${server.httpServer.address().port}/sequence-check`,
      );
      await expect(page.locator("html")).toHaveAttribute(
        "data-viewport-surface",
        "",
      );
      await page.locator("#message").focus();
      if (name === "transformed root")
        await page.locator("#root").evaluate((n) => {
          n.style.transform = "translateZ(0)";
        });
      for (const step of steps) {
        const frame = await page.evaluate(
          (patch) => window.stepKeyboard(patch),
          step,
        );
        expect(frame.top).toBe(frame.visibleTop);
        expect(frame.rootHeight).toBe(669);
        expect(frame.composerTop).toBeGreaterThanOrEqual(frame.visibleTop);
        expect(frame.composerBottom).toBeLessThanOrEqual(
          frame.visibleTop + frame.visibleHeight,
        );
      }
    } finally {
      await page.close();
      await server.close();
    }
  });
