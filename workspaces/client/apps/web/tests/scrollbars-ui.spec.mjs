import { createRequire } from "node:module";
const { test, expect } = createRequire(import.meta.url)("@playwright/test");
import { build } from "esbuild";
import { fileURLToPath } from "node:url";
import { browserExecutablePath } from "./browser-executable.mjs";

test.use({
  launchOptions: {
    executablePath: browserExecutablePath,
    ignoreDefaultArgs: ["--hide-scrollbars"],
  },
});

const root = fileURLToPath(new URL("../", import.meta.url));

// Exercise the real CSS cascade and interactive Mantine component without a backend.
test("native panes and Mantine share scrollbar styling in both themes", async ({
  page,
}, testInfo) => {
  const bundle = await build({
    absWorkingDir: root,
    stdin: {
      resolveDir: root,
      loader: "jsx",
      contents: `
        import React from 'react';
        import { createRoot } from 'react-dom/client';
        import { createPortal } from 'react-dom';
        import { MantineProvider, ScrollArea } from '@mantine/core';
        import { theme } from './src/theme';
        import '@mantine/core/styles.css';
        import './src/style.css';
        import './src/components/sidebar-projects.css';
        const content = <div style={{height: 1200, width: 800}}>Scrollable content</div>;
        createRoot(document.getElementById('fixture')).render(
          <MantineProvider theme={theme}>
            <div id="messages" tabIndex={0} style={{height:180,width:260,flex:'none',padding:8}}>{content}</div>
            <div className="chat-scroll" tabIndex={0} style={{height:180,width:260,flex:'none'}}>{content}</div>
            <ScrollArea type="always" style={{height:180,width:260}}>{content}</ScrollArea>
            {createPortal(<textarea aria-label="Portal draft" defaultValue={'Draft line\\n'.repeat(100)} style={{height:180,width:260}} />, document.body)}
          </MantineProvider>
        );`,
    },
    bundle: true,
    write: false,
    outdir: testInfo.outputPath("scrollbar-bundle"),
    define: { "process.env.NODE_ENV": '"production"' },
  });
  await page.setContent(
    '<div id="fixture" style="display:flex;gap:24px;padding:24px"></div>',
  );
  await page.addStyleTag({
    content: bundle.outputFiles.find((file) => file.path.endsWith(".css")).text,
  });
  await page.addScriptTag({
    content: bundle.outputFiles.find((file) => file.path.endsWith(".js")).text,
  });
  const viewport = page.locator(".mantine-ScrollArea-viewport");
  const verticalTrack = page.locator(
    '.mantine-ScrollArea-scrollbar[data-orientation="vertical"]',
  );
  const thumb = verticalTrack.locator(".mantine-ScrollArea-thumb");
  await expect(thumb).toBeVisible();
  await expect(thumb).toHaveCSS("width", "6px");
  await expect(thumb).toHaveCSS("border-radius", "12px");
  await expect(verticalTrack).toHaveCSS("width", "12px");
  await expect(
    page.locator(
      '.mantine-ScrollArea-scrollbar[data-orientation="horizontal"] > .mantine-ScrollArea-thumb',
    ),
  ).toHaveCSS("height", "6px");

  for (const scheme of ["light", "dark"]) {
    await page
      .locator("html")
      .evaluate(
        (element, value) =>
          element.setAttribute("data-mantine-color-scheme", value),
        scheme,
      );
    const expectedColor =
      scheme === "light"
        ? "rgba(24, 36, 71, 0.094)"
        : "rgba(255, 255, 255, 0.094)";
    for (const selector of ["#messages", ".chat-scroll", "textarea"]) {
      await expect(page.locator(selector)).toHaveCSS("scrollbar-width", "thin");
      await expect(page.locator(selector)).toHaveCSS(
        "scrollbar-color",
        `${expectedColor} rgba(0, 0, 0, 0)`,
      );
    }
    await expect(viewport).toHaveCSS("scrollbar-width", "none");
    await expect(thumb).toHaveCSS("background-color", expectedColor);
    await verticalTrack.hover();
    await expect(verticalTrack).toHaveCSS(
      "background-color",
      "rgba(0, 0, 0, 0)",
    );
    await expect(thumb).toHaveCSS("background-color", expectedColor);
    await page.mouse.down();
    await expect(verticalTrack).toHaveCSS(
      "background-color",
      "rgba(0, 0, 0, 0)",
    );
    await expect(thumb).toHaveCSS("background-color", expectedColor);
    await page.mouse.up();
    await expect(page.locator(".mantine-ScrollArea-corner")).toHaveCSS(
      "background-color",
      "rgba(0, 0, 0, 0)",
    );
    await testInfo.attach(`${scheme}-computed`, {
      body: JSON.stringify(
        await page.evaluate(() => {
          const properties = [
            "scrollbarColor",
            "scrollbarWidth",
            "backgroundColor",
            "width",
            "borderRadius",
            "overflowY",
            "overflowX",
          ];
          return Object.fromEntries(
            [
              "#messages",
              ".chat-scroll",
              "textarea",
              ".mantine-ScrollArea-viewport",
              '.mantine-ScrollArea-scrollbar[data-orientation="vertical"]',
              '.mantine-ScrollArea-scrollbar[data-orientation="vertical"] > .mantine-ScrollArea-thumb',
            ].map((selector) => {
              const style = getComputedStyle(document.querySelector(selector));
              return [
                selector,
                Object.fromEntries(properties.map((key) => [key, style[key]])),
              ];
            }),
          );
        }),
        null,
        2,
      ),
      contentType: "application/json",
    });
    await testInfo.attach(`${scheme}-scrollbars`, {
      body: await page.screenshot(),
      contentType: "image/png",
    });
  }

  await page.locator(".chat-scroll").hover();
  await page.mouse.wheel(0, 150);
  await expect
    .poll(() =>
      page.locator(".chat-scroll").evaluate((element) => element.scrollTop),
    )
    .toBeGreaterThan(0);
  await page.locator("#messages").focus();
  await page.keyboard.press("ArrowDown");
  await expect
    .poll(() =>
      page.locator("#messages").evaluate((element) => element.scrollTop),
    )
    .toBeGreaterThan(0);
  const box = await thumb.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2 + 60);
  await page.mouse.up();
  await expect
    .poll(() => viewport.evaluate((element) => element.scrollTop))
    .toBeGreaterThan(0);
});
