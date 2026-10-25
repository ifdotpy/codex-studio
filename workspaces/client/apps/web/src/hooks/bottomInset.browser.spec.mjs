import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../tests/playwright.mjs";

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
        resolveId(id) {
          if (id === "/bottom-entry") return "\0bottom-entry";
        },
        load(id) {
          if (id !== "\0bottom-entry") return;
          return `import React from 'react'; import {createRoot} from 'react-dom/client';
        import {MantineProvider} from '@mantine/core'; import {theme} from '/src/theme.ts';
        import {useMobileViewport} from '/src/hooks/mobileViewport.ts';
        import {BottomInsetBand,PassiveUsage} from '/src/hooks/BottomInsetBand.tsx';
        import '@mantine/core/styles.css'; import '/src/style.css'; import '/src/components/Usage.css';
        import '/src/hooks/viewport-surface.css';
        const e=React.createElement;
        function Fixture(){ useMobileViewport(true); return e(MantineProvider,{theme},e(BottomInsetBand,null,
          e('aside',{id:'sidebar','data-presentation':'inline',style:{width:226,background:'var(--sidebar)'}},'Chats'),
          e('main',{className:'workspace',style:{display:'flex',flexDirection:'column',flex:1}},
            e('div',{style:{flex:1}},'Conversation'),
            e('form',{id:'composer',style:{height:100,flexShrink:0}},e('textarea',{id:'message',defaultValue:'Draft'}),e('button',null,'Send')),
            e('div',{className:'usage-footer',style:{flex:'0 0 auto'}},
              e(PassiveUsage,null,e('div',{className:'session-cost-summary'},'Session estimate: $3,589.76'),
              e('span',{className:'token-rate'},'42 tok/s')),
              e('button',{className:'context-use'},'Context 39%'),e('button',null,'Account'),e('button',null,'Limits'))))); }
        createRoot(document.querySelector('#root')).render(e(Fixture));`;
        },
        configureServer(s) {
          s.middlewares.use((req, res, next) => {
            if (req.url !== "/bottom-check") return next();
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover"><div id="root"></div><script type="module" src="/bottom-entry"></script>',
            );
          });
        },
      },
    ],
  });
  const page = await browser.newPage({
    viewport: { width: 951, height: 669 },
    hasTouch: false,
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
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/bottom-check`,
    );
    expect(
      await page.evaluate(() => matchMedia("(any-pointer: coarse)").matches),
    ).toBe(false);
    await expect(page.locator("#message")).toHaveCSS("font-size", "16px");
    const status = page.locator(".session-cost-summary");
    await expect(status).toBeVisible();
    const before = await status.boundingBox();
    console.log(
      "passive-status-before-check",
      JSON.stringify(before),
      "empty-band-height",
      34,
    );
    await expect
      .poll(async () => (await status.boundingBox()).y)
      .toBeGreaterThanOrEqual(635);
    const rect = await status.boundingBox();
    expect(rect.y + rect.height).toBeLessThanOrEqual(669);
    await expect(page.locator(".token-rate")).toBeVisible();
    for (const target of await page.locator("button,textarea").all()) {
      const box = await target.boundingBox();
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
