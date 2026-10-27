import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../tests/playwright.mjs";

test("wide touch viewport keeps the root and portals inside all safe edges", async ({
  browser,
  browserName,
}) => {
  test.skip(
    browserName !== "chromium",
    "The inset fixture requires Chromium emulation.",
  );
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-viewport-surface-"));
  const entry = `
    import React from 'react';
    import {createRoot} from 'react-dom/client';
    import {MantineProvider, Drawer, Modal} from '@mantine/core';
    import '@mantine/core/styles.css';
    import '/src/style.css';
    import '/src/hooks/viewport-surface.css';
    import {useMobileViewport} from '/src/hooks/mobileViewport.ts';
    function ViewportOwner({active}) { useMobileViewport(active, false); return null; }
    function Surface(){
      const [owner,setOwner]=React.useState(1);
      window.selectViewportOwner=setOwner;
      const [portal,setPortal]=React.useState('');
      window.openPortal=setPortal;
      return React.createElement(MantineProvider,null,
        React.createElement(ViewportOwner,{active:owner===1}),
        React.createElement(ViewportOwner,{active:owner===2}),
        React.createElement('main',{id:'viewport-content',style:{flex:1,minWidth:0,minHeight:0,display:'flex',flexDirection:'column'}},
          React.createElement('header',null,'Header'),
          React.createElement('div',{style:{flex:1}},'Conversation'),
          React.createElement('footer',null,'Composer')),
        React.createElement(Drawer,{opened:portal==='drawer',onClose:()=>setPortal(''),position:'right',size:'100%',title:'Drawer'},React.createElement('button',null,'Drawer footer')),
        React.createElement(Modal,{opened:portal==='modal',onClose:()=>setPortal(''),fullScreen:true,title:'Modal'},React.createElement('button',null,'Modal footer')));
    }
    const root=createRoot(document.querySelector('#root'));
    root.render(React.createElement(Surface));
    window.unmountSurface=()=>root.unmount();
  `;
  const server = await createFixtureViteServer({
    configFile: false,
    cacheDir,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "viewport-surface-fixture",
        resolveId(id) {
          if (id === "/viewport-entry") return "\0viewport-entry";
        },
        load(id) {
          if (id === "\0viewport-entry") return entry;
        },
        configureServer(server) {
          server.middlewares.use((request, response, next) => {
            if (request.url !== "/viewport-check") return next();
            response.setHeader("Content-Type", "text/html");
            response.end(
              '<!doctype html><meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover"><div id="root"></div><script type="module" src="/viewport-entry"></script>',
            );
          });
        },
      },
    ],
  });
  const page = await browser.newPage({
    viewport: { width: 951, height: 669 },
    hasTouch: true,
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(() => {
    const viewport = Object.assign(new EventTarget(), {
      width: 951,
      height: 669,
      offsetTop: 0,
      scale: 1,
    });
    Object.defineProperty(window, "visualViewport", {
      configurable: true,
      value: viewport,
    });
    const observers = new Set();
    const add = viewport.addEventListener.bind(viewport);
    const remove = viewport.removeEventListener.bind(viewport);
    viewport.addEventListener = (type, listener, ...rest) => {
      if (type === "resize") observers.add(listener);
      add(type, listener, ...rest);
    };
    viewport.removeEventListener = (type, listener, ...rest) => {
      if (type === "resize") observers.delete(listener);
      remove(type, listener, ...rest);
    };
    window.viewportObserverCount = () => observers.size;
    window.resizeViewport = (patch) => {
      Object.assign(viewport, patch);
      viewport.dispatchEvent(new Event("resize"));
    };
  });
  try {
    await server.listen();
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 0, right: 84, bottom: 34, left: 0 },
    });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/viewport-check`,
    );
    await expect(page.locator("#viewport-content")).toBeVisible();
    const bounds = (selector) =>
      page.locator(selector).evaluate((node) => {
        const rect = node.getBoundingClientRect();
        return {
          left: rect.left,
          top: rect.top,
          right: rect.right,
          bottom: rect.bottom,
          height: rect.height,
        };
      });
    await expect
      .poll(() => bounds("#viewport-content"))
      .toEqual({ left: 0, top: 0, right: 867, bottom: 635, height: 635 });
    await page.screenshot({ path: test.info().outputPath("wide-touch.png") });
    expect(await page.evaluate(() => window.viewportObserverCount())).toBe(1);
    await page.evaluate(() => window.selectViewportOwner(2));
    await expect
      .poll(() => page.evaluate(() => window.viewportObserverCount()))
      .toBe(1);
    await expect
      .poll(() => bounds("#viewport-content"))
      .toMatchObject({ right: 867, bottom: 635 });
    for (const portal of ["drawer", "modal"]) {
      await page.evaluate((portal) => window.openPortal(portal), portal);
      const selector =
        portal === "drawer"
          ? ".mantine-Drawer-content"
          : ".mantine-Modal-content";
      await expect(page.locator(selector)).toBeVisible();
      await expect
        .poll(() => bounds(selector))
        .toMatchObject({ left: 0, top: 0, right: 867, bottom: 635 });
      await page.evaluate(() =>
        window.resizeViewport({ height: 350, offsetTop: 20 }),
      );
      await expect
        .poll(() => bounds(selector))
        .toMatchObject({ top: 20, bottom: 655 });
      await expect
        .poll(() => bounds("#viewport-content"))
        .toMatchObject({ left: 0, right: 867, top: 20, bottom: 655 });
      await page.evaluate(() => window.openPortal(""));
      await expect(page.locator(selector)).toHaveCount(0);
      await page.evaluate(() =>
        window.resizeViewport({ height: 669, offsetTop: 0 }),
      );
    }
    // Physical insets remain independent and the top offset reduces top padding once.
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 59, right: 84, bottom: 34, left: 12 },
    });
    await page.evaluate(() =>
      window.resizeViewport({ height: 620, offsetTop: 12 }),
    );
    await expect
      .poll(() => bounds("#viewport-content"))
      .toMatchObject({ left: 12, right: 867, top: 59, bottom: 598 });
    await page.evaluate(() =>
      window.resizeViewport({ height: 310, offsetTop: 100, scale: 2 }),
    );
    await expect
      .poll(() => bounds("#viewport-content"))
      .toMatchObject({ left: 12, right: 867, top: 59, bottom: 598 });
    await page.evaluate(() => window.unmountSurface());
    expect(
      await page.evaluate(() =>
        document.documentElement.hasAttribute("data-viewport-surface"),
      ),
    ).toBe(false);
    expect(await page.evaluate(() => window.viewportObserverCount())).toBe(0);
    await page.addInitScript(() => {
      Object.defineProperty(window, "visualViewport", {
        configurable: true,
        value: undefined,
      });
    });
    await page.reload();
    await expect(page.locator("#viewport-content")).toBeVisible();
    await expect
      .poll(() => bounds("#viewport-content"))
      .toEqual({ left: 12, top: 59, right: 867, bottom: 635, height: 576 });
    // iOS 27.2 Duo shell: WebKit reports the side strip as top 82; the
    // shell's lengths replace env() on all four sides.
    await cdp.send("Emulation.setSafeAreaInsetsOverride", {
      insets: { top: 82, right: 0, bottom: 34, left: 0 },
    });
    await page.evaluate(() => {
      const style = document.documentElement.style;
      style.setProperty("--native-safe-top", "0px");
      style.setProperty("--native-safe-right", "82px");
      style.setProperty("--native-safe-bottom", "34px");
      style.setProperty("--native-safe-left", "0px");
    });
    await expect
      .poll(() => bounds("#viewport-content"))
      .toEqual({ left: 0, top: 0, right: 869, bottom: 635, height: 635 });
    expect(errors).toEqual([]);
  } finally {
    await page.close();
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
