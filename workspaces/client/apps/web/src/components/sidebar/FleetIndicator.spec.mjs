import {
  test,
  expect,
  rustUiFixture,
  spawnFixture as spawn,
} from "../../../tests/playwright.mjs";
import { mkdir, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Built renderer, disposable Rust fixture, scripted provider; no live user state.
test("fleet state indicators remain visible and animated in priority and project rows", async ({
  browser,
}) => {
  test.setTimeout(90000);
  const root = await mkdtemp(join(tmpdir(), "studio-fleet-indicator-"));
  const evidence =
    process.env.FLEET_INDICATOR_EVIDENCE || join(root, "screenshots");
  await mkdir(evidence, { recursive: true });
  const proc = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      FIXTURE_SEED: "question",
      FIXTURE_HOLD: "lead,other",
      FIXTURE_WORKER_COUNT: "0",
    },
  });
  let errors = "";
  proc.stderr.on("data", (chunk) => {
    errors += chunk;
  });
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (chunk) => resolve(Number(String(chunk).trim())));
    proc.once("exit", () => reject(new Error(errors)));
  });
  try {
    for (const scheme of ["light", "dark"]) {
      const context = await browser.newContext({ colorScheme: scheme });
      const page = await context.newPage();
      await page.addInitScript(
        (scheme) => localStorage.setItem("mantine-color-scheme-value", scheme),
        scheme,
      );
      await page.setViewportSize({ width: 1440, height: 960 });
      for (const width of [1440, 390]) {
        await page.setViewportSize({ width, height: 960 });
        await page.goto(`http://127.0.0.1:${port}`);
        await expect(page.locator("#composer")).toBeVisible();
        if (!(await page.locator("#sidebar").isVisible()))
          await page.locator("#sidebar-toggle").click();
        const sidebar = page.locator("#sidebar");
        await expect(sidebar).toBeVisible();
        const lane = sidebar.getByRole("region", { name: "Needs you" });
        await expect(lane.locator('[data-chat-status="answer"]')).toBeVisible();
        const working = sidebar
          .locator('.sidebar-project [data-chat-status="working"]')
          .first();
        await expect(working).toBeVisible();
        for (const status of [
          lane.locator('[data-chat-status="answer"]').first(),
          working,
        ]) {
          await status.scrollIntoViewIfNeeded();
          const ratio = await status.evaluate(
            (element) =>
              new Promise((resolve) => {
                const observer = new IntersectionObserver(([entry]) => {
                  observer.disconnect();
                  resolve(entry.intersectionRatio);
                });
                observer.observe(element);
              }),
          );
          expect(ratio).toBeGreaterThan(0.9);
          await expect(status).toHaveAccessibleName(/.+/);
        }
        await expect(working.locator("svg")).toHaveCSS(
          "animation-name",
          "chat-status-spin",
        );
        await expect(
          sidebar
            .locator(
              '.sidebar-project .fleet-strip[data-studio-state="working"]',
            )
            .first(),
        ).toHaveCSS("box-shadow", "none");
        expect(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
          ),
        ).toBe(true);
        await sidebar.locator("#chat-list").evaluate((element) => {
          element.scrollTop = 0;
        });
        await page.evaluate(() => document.activeElement?.blur());
        await page.screenshot({
          path: join(evidence, `indicator-real-${scheme}-${width}.png`),
        });
        await page.emulateMedia({ reducedMotion: "reduce" });
        await expect(working.locator("svg")).toHaveCSS(
          "animation-name",
          "none",
        );
        await page.emulateMedia({ reducedMotion: "no-preference" });
      }
      await context.close();
    }
  } finally {
    proc.kill("SIGTERM");
  }
});
