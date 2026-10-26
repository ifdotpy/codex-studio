import { waitForFixturePort } from "../../tests/fixture-readiness.ts";
import {
  test,
  expect,
  rustUiFixture,
  spawnFixture as spawn,
} from "../../tests/playwright.ts";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("Foundation content width and fleet geometry", async ({ page }) => {
  test.setTimeout(90000);
  const root = await mkdtemp(join(tmpdir(), "studio-foundation-geometry-"));
  const proc = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, FIXTURE_SEED: "report", FIXTURE_WORKER_COUNT: "2" },
  });
  let errors = "";
  proc.stderr?.on("data", (chunk) => {
    errors += chunk;
  });
  const port = await waitForFixturePort(proc, { diagnostics: () => errors });
  try {
    await page.setViewportSize({ width: 1280, height: 960 });
    await page.goto(`http://127.0.0.1:${port}`);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await page.locator("#composer").waitFor();
    for (const width of [1024, 1280]) {
      await page.setViewportSize({ width, height: 960 });
      for (const ratio of [1, 0.65]) {
        await page.evaluate((ratio) => {
          document.documentElement.dataset.studioContentLayout = "custom";
          document.documentElement.style.setProperty(
            "--studio-content-width-ratio",
            String(ratio),
          );
        }, ratio);
        const geometry = await page.evaluate(() => {
          const element = (selector: string) => {
            const node = document.querySelector(selector);
            if (!node) throw new Error(`Missing geometry element: ${selector}`);
            return node;
          };
          const rect = (selector: string) =>
            element(selector).getBoundingClientRect().toJSON();
          return {
            sidebar: rect("#sidebar"),
            conversation: rect("#conversation"),
            composer: rect("#composer"),
            message: rect(".message-content"),
            gutter: parseFloat(
              getComputedStyle(element(".workspace")).getPropertyValue(
                "--chat-gutter",
              ),
            ),
            scrollWidth: document.documentElement.scrollWidth,
            viewport: innerWidth,
          };
        });
        expect(geometry.sidebar.width).toBe(288);
        expect(geometry.scrollWidth).toBeLessThanOrEqual(width);
        const available =
          (geometry.conversation.width - 2 * geometry.gutter) * ratio;
        expect(Math.abs(geometry.composer.width - available)).toBeLessThan(2);
        expect(geometry.composer.right).toBeLessThanOrEqual(width);
        expect(geometry.message.right).toBeLessThanOrEqual(width);
        console.log(JSON.stringify({ width, ratio, ...geometry }));
      }
    }
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.locator("#composer")).toBeVisible();
    await page.locator("#sidebar-toggle").click();
    await expect(page.locator("#sidebar")).toBeVisible();
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await expect(page.locator("#composer")).toBeVisible();
  } finally {
    proc.kill("SIGTERM");
  }
});
