import { join } from "node:path";
import {
  test,
  expect,
  createFixtureViteServer,
  apiSchemaHeaders,
} from "../playwright.mjs";

// Real settings and transcript notice components, with an isolated API fixture.
test("Claude idle compaction settings and notice at 320 and 1200 pixels", async ({
  page,
}, testInfo) => {
  test.setTimeout(60000);
  const root = join(import.meta.dirname, "../../");
  const entry = join(root, "idle-compaction-fixture.tsx");
  const notice =
    "Compacted while idle, before the prompt cache expired. To turn it off, set Claude idle compaction to Off in Settings > Agent orchestration.";
  const deferredNotice =
    "Compacted on return with a summary prepared while idle. To turn it off, set Claude idle compaction to Off in Settings > Agent orchestration.";
  let settings = {
    childResultCoalesceSeconds: 10,
    idleSessionUnloadMinutes: 15,
    idleCompaction: { mode: "off", minTokens: 150000 },
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
        name: "idle-compaction-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/idle-compaction-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/idle-compaction-fixture.tsx") return entry;
        },
        load(id) {
          if (id !== entry) return;
          return `import React from 'react';import {createRoot} from 'react-dom/client';import {MantineProvider} from '@mantine/core';import '@mantine/core/styles.css';import {OrchestrationSettings} from '/src/components/OrchestrationSettings.tsx';import {IdleCompactionNotice} from '/src/components/IdleCompactionNotice.tsx';import '/src/components/ui/primitives.css';
      createRoot(document.getElementById('root')).render(<MantineProvider><main style={{padding:16,maxWidth:900,margin:'auto'}}><OrchestrationSettings active={true}/><section aria-label="Transcript"><h2>Transcript</h2><IdleCompactionNotice text={${JSON.stringify(notice)}}/><IdleCompactionNotice text={${JSON.stringify(deferredNotice)}}/></section></main></MantineProvider>);`;
        },
      },
    ],
  });
  try {
    await server.listen();
    const url = `http://127.0.0.1:${server.httpServer.address().port}/check`;
    for (const width of [320, 1200]) {
      await page.setViewportSize({ width, height: 900 });
      await page.goto(url);
      const mode = page.getByLabel("Claude idle compaction", { exact: true });
      await expect(mode).toHaveValue(settings.idleCompaction.mode);
      await mode.selectOption("log");
      await page
        .getByRole("button", { name: "Save orchestration settings" })
        .click();
      await expect.poll(() => saved.at(-1)?.idleCompaction.mode).toBe("log");
      await mode.selectOption("compact");
      const tokens = page.getByLabel("Idle compaction minimum context tokens");
      await tokens.fill("60000");
      await tokens.blur();
      await page
        .getByRole("button", { name: "Save orchestration settings" })
        .click();
      await expect
        .poll(() => saved.at(-1)?.idleCompaction)
        .toEqual({ mode: "compact", minTokens: 60000 });
      await mode.selectOption("compact_on_return");
      await page
        .getByRole("button", { name: "Save orchestration settings" })
        .click();
      await expect
        .poll(() => saved.at(-1)?.idleCompaction)
        .toEqual({ mode: "compact_on_return", minTokens: 60000 });
      await expect(page.getByRole("status")).toHaveText([
        notice,
        deferredNotice,
      ]);
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= window.innerWidth,
        ),
      ).toBe(true);
      await page.screenshot({
        path: testInfo.outputPath(`idle-compaction-${width}.png`),
        fullPage: true,
      });
      await mode.selectOption("off");
      await page
        .getByRole("button", { name: "Save orchestration settings" })
        .click();
      await expect.poll(() => saved.at(-1)?.idleCompaction.mode).toBe("off");
    }
  } finally {
    await server.close();
  }
});
