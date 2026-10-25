import { join } from "node:path";
import {
  test,
  expect,
  createFixtureViteServer,
  apiSchemaHeaders,
} from "../playwright.mjs";

test("background shell settings and cleanup notice at 320 and 1200 pixels", async ({
  page,
}, testInfo) => {
  test.setTimeout(60000);
  const root = join(import.meta.dirname, "../../");
  const entry = join(root, "background-shell-fixture.tsx");
  const notice =
    "Critical memory pressure: Studio stopped background shell shell-1 after 30 idle minutes.";
  let settings = {
    childResultCoalesceSeconds: 10,
    idleSessionUnloadMinutes: 15,
    shellPolicy: { quietThresholdMs: 0, pressureCleanup: true },
  };
  const saved = [];
  await page.route("**/api/orchestration/settings", async (route) => {
    if (route.request().method() === "POST") {
      settings = route.request().postDataJSON();
      saved.push(settings);
    }
    await route.fulfill({
      status: 200,
      headers: apiSchemaHeaders(),
      json: { settings },
    });
  });
  const server = await createFixtureViteServer({
    root,
    configFile: false,
    server: { host: "127.0.0.1", port: 0 },
    plugins: [
      {
        name: "background-shell-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/background-shell-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/background-shell-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React from 'react';import {createRoot} from 'react-dom/client';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import {OrchestrationSettings} from '/src/components/OrchestrationSettings.tsx';import {NativeNotice} from '/src/components/NativeNotice.tsx';import '/src/components/ui/primitives.css';
        createRoot(document.getElementById('root')).render(<MantineProvider><main style={{padding:16,maxWidth:900,margin:'auto'}}><OrchestrationSettings active={true}/><section aria-label="Transcript"><h2>Transcript</h2><NativeNotice item={{id:'pressure',role:'system',text:${JSON.stringify(notice)},nativeNotice:'info'}}/></section></main></MantineProvider>);`;
        },
      },
    ],
  });
  try {
    await server.listen();
    const url = `http://127.0.0.1:${server.httpServer.address().port}/check`;
    for (const width of [320, 1200]) {
      await page.setViewportSize({ width, height: 1100 });
      await page.goto(url);
      const threshold = page.getByLabel(
        "Background shell quiet threshold in milliseconds",
      );
      await expect(threshold).toHaveValue(
        String(settings.shellPolicy.quietThresholdMs),
      );
      await threshold.fill("10000");
      await threshold.blur();
      const cleanup = page.getByRole("switch", {
        name: "Stop idle shells under critical memory pressure",
      });
      await cleanup.uncheck();
      await page
        .getByRole("button", { name: "Save orchestration settings" })
        .click();
      await expect
        .poll(() => saved.at(-1)?.shellPolicy)
        .toEqual({ quietThresholdMs: 10000, pressureCleanup: false });
      await page.reload();
      await expect(threshold).toHaveValue("10000");
      await expect(cleanup).not.toBeChecked();
      await expect(page.getByText(notice, { exact: true })).toBeVisible();
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth,
        ),
      ).toBe(true);
      await page.screenshot({
        path: testInfo.outputPath(`background-shell-${width}.png`),
        fullPage: true,
      });
    }
  } finally {
    await server.close();
  }
});
