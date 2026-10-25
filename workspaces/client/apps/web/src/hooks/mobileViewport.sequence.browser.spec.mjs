import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../tests/playwright.mjs";

import {
  keyboardSequences,
  keyboardCycleFrames,
} from "../../tests/layout/keyboardSequences.mjs";

for (const [name, steps] of Object.entries(keyboardSequences))
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
 const e=React.createElement;function Fixture(){useMobileViewport(true);return e(React.Fragment,null,e('aside',{id:'sidebar',style:{width:226,height:'100%',flex:'none'}},'Chats'),e('section',{style:{height:'100%',flex:1,display:'flex',flexDirection:'column'}},e('header',{style:{height:44,flex:'none'}},'Header'),e('main',{id:'conversation',style:{display:'flex',flex:1,minHeight:0,flexDirection:'column'}},e('div',{id:'messages',style:{flex:1,minHeight:0,overflow:'auto'}},'Transcript '.repeat(100)),e('form',{id:'composer',style:{height:${name === "device final" ? 91 : 120},flex:'none',margin:0}},e('textarea',{id:'message',defaultValue:'Draft'})))));}createRoot(document.querySelector('#root')).render(e(Fixture));`;
          },
          configureServer(s) {
            s.middlewares.use((req, res, next) => {
              if (req.url !== "/sequence-check") return next();
              res.setHeader("Content-Type", "text/html");
              res.end(
                '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover"><style>*{box-sizing:border-box}body{margin:0}#root{display:flex;flex-direction:row}#composer{background:white}textarea{font-size:16px}</style><div id="root"></div><script type="module" src="/sequence-entry"></script>',
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
                r.y - viewport.offsetTop,
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
            const visibleTop = visualOrigin ? 0 : viewport.offsetTop;
            const screenRect = (selector) => {
              const r = document
                .querySelector(selector)
                .getBoundingClientRect();
              return {
                x: r.x,
                y: r.y - visibleTop,
                width: r.width,
                height: r.height,
              };
            };
            return {
              sidebar: screenRect("#sidebar"),
              header: screenRect("header"),
              offsetTop: viewport.offsetTop,
              pageTop: viewport.pageTop,
              scrollY,
              top: root.top,
              rootHeight: root.height,
              composerTop: composer.top,
              composerBottom: composer.bottom,
              visibleTop: visualOrigin ? 0 : viewport.offsetTop,
              visibleHeight: viewport.height,
            };
          };
        },
        { visualOrigin: name === "visual origin" || name === "device final" },
      );
      const cdp = await page.context().newCDPSession(page);
      await cdp.send("Emulation.setSafeAreaInsetsOverride", {
        insets: { top: 0, right: 0, bottom: 34, left: 0 },
      });
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
      const baseline = await page.evaluate(() => window.stepKeyboard({}));
      const records = [];
      const frames = keyboardCycleFrames(steps);
      for (const [index, step] of frames.entries()) {
        const frame = await page.evaluate(
          (patch) => window.stepKeyboard(patch),
          step,
        );
        records.push({ index, step, ...frame });
        expect(frame.sidebar, `${name} frame ${index} sidebar`).toEqual(
          baseline.sidebar,
        );
        expect(frame.header, `${name} frame ${index} header`).toEqual(
          baseline.header,
        );
        if (
          name === "device final" &&
          step.height === 323 &&
          step.offsetTop === 346
        ) {
          expect(frame.offsetTop).toBe(346);
          expect(frame.pageTop).toBe(346);
          expect(frame.scrollY).toBe(346);
          expect(frame.top).toBe(0);
          expect(frame.composerTop).toBe(232);
          expect(frame.composerBottom).toBe(323);
        }
        expect(frame.top).toBe(frame.visibleTop);
        expect(frame.rootHeight).toBe(669);
        expect(frame.composerTop).toBeGreaterThanOrEqual(frame.visibleTop);
        expect(frame.composerBottom).toBeLessThanOrEqual(
          frame.visibleTop + frame.visibleHeight,
        );
      }
      await test.info().attach("keyboard-frames", {
        body: JSON.stringify(records, null, 2),
        contentType: "application/json",
      });
    } finally {
      await page.close();
      await server.close();
    }
  });
