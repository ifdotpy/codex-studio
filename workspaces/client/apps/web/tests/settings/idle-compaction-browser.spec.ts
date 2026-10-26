import { required } from "./fixture-helpers.ts";
import type { components } from "../../src/generated/api.ts";
import { fixtureUrl } from "./fixture-helpers.ts";
import type {} from "./idle-compaction-browser.fixture.tsx";
import { join } from "node:path";
import {
  test,
  expect,
  createFixtureViteServer,
  apiSchemaHeaders,
} from "../playwright.ts";

// Real settings and transcript notice components, with an isolated API fixture.
test("Claude idle compaction settings and notice at 320 and 1200 pixels", async ({
  page,
}, testInfo) => {
  test.setTimeout(60000);
  const root = join(import.meta.dirname, "../../");
  const notice =
    "Compacted while idle, before the prompt cache expired. To turn it off, set Claude idle compaction to Off in Settings > Agent orchestration.";
  let settings: components["schemas"]["OrchestrationSettings"] = {
    childResultCoalesceSeconds: 10,
    idleSessionUnloadMinutes: 15,
    idleCompaction: { mode: "off", minTokens: 200000 },
  };
  const saved: (typeof settings)[] = [];
  await page.route("**/api/orchestration/settings", async (route) => {
    if (route.request().method() === "POST") {
      settings = route.request().postDataJSON();
      saved.push(settings);
    }
    await route.fulfill({
      status: 200,
      headers: apiSchemaHeaders(),
      json: {
        settings,
      } satisfies components["schemas"]["OrchestrationSettingsResponse"],
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
              '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/tests/settings/idle-compaction-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const url = fixtureUrl(server, "check");
    for (const width of [320, 1200]) {
      await page.setViewportSize({ width, height: 900 });
      await page.goto(url);
      const mode = page.getByLabel("Claude idle compaction", { exact: true });
      await expect(mode).toHaveValue(required(settings.idleCompaction).mode);
      await mode.selectOption("log");
      await page
        .getByRole("button", { name: "Save orchestration settings" })
        .click();
      await expect.poll(() => saved.at(-1)?.idleCompaction?.mode).toBe("log");
      await mode.selectOption("compact");
      const tokens = page.getByLabel("Idle compaction minimum context tokens");
      await tokens.fill("100000");
      await tokens.blur();
      await page
        .getByRole("button", { name: "Save orchestration settings" })
        .click();
      await expect
        .poll(() => saved.at(-1)?.idleCompaction)
        .toEqual({ mode: "compact", minTokens: 100000 });
      await expect(page.getByRole("status")).toHaveText(notice);
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
      await expect.poll(() => saved.at(-1)?.idleCompaction?.mode).toBe("off");
    }
  } finally {
    await server.close();
  }
});
