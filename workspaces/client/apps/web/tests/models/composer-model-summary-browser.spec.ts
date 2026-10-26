import { fixtureOrigin } from "../misc-fixture.ts";
import type {} from "./composer-model-summary-browser.fixture.tsx";
import { join } from "node:path";
import { mkdir, writeFile } from "node:fs/promises";
import { createFixtureViteServer, test, expect } from "../playwright.ts";

// Use the composer's real trigger, settings caller, toolbar controls and CSS.
// Only the catalog is a fixture; opening and switching roles use production UI.
test("composer model summary wraps on phones and opens the role settings", async ({
  page,
}) => {
  test.setTimeout(120000);
  const root = join(import.meta.dirname, "../..");
  const server = await createFixtureViteServer({
    root,
    configFile: false,
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "composer-model-summary-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/tests/models/composer-model-summary-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  await server.listen();
  try {
    const screenshots = process.env.COMPOSER_SUMMARY_SCREENSHOTS;
    if (screenshots) await mkdir(screenshots, { recursive: true });
    const measurements = [];
    const url = fixtureOrigin(server) + "/check";
    for (const width of [320, 360, 390, 1440]) {
      await page.setViewportSize({ width, height: 844 });
      for (const scenario of [
        "long=word&worker=1",
        "long=words&active=1",
        "",
      ]) {
        await page.goto(`${url}?${scenario}`);
        const trigger = page.getByRole("button", {
          name: /^Execution settings:/,
        });
        await expect(trigger).toBeVisible();
        await expect(trigger).toHaveAccessibleName(/Review same as caller/);
        const filename = `composer-${width}-${scenario.replaceAll(/[^a-z]/g, "") || "normal"}.png`;
        await page.screenshot({
          path: screenshots
            ? join(screenshots, filename)
            : test.info().outputPath(filename),
        });
        const geometry = await trigger.evaluate((node) => {
          const bounds = node.getBoundingClientRect();
          const texts = [
            ...node.querySelectorAll(
              ".execution-model,.execution-inherited-label",
            ),
          ].filter((el) => el.getBoundingClientRect().height);
          return {
            pageWidth: document.documentElement.scrollWidth,
            viewport: innerWidth,
            overflow: node.scrollWidth - node.clientWidth,
            escaped: texts.some((el) => {
              const r = el.getBoundingClientRect();
              return r.left < bounds.left || r.right > bounds.right + 1;
            }),
            // Detect overlaps with the actual neighboring toolbar controls.
            overlap: [...document.querySelectorAll("#composer button")]
              .filter((el) => el !== node)
              .some((el) => {
                const r = el.getBoundingClientRect();
                return (
                  r.width &&
                  r.left < bounds.right &&
                  r.right > bounds.left &&
                  r.top < bounds.bottom &&
                  r.bottom > bounds.top
                );
              }),
          };
        });
        expect(
          geometry,
          JSON.stringify({ width, scenario, ...geometry }),
        ).toMatchObject({ overflow: 0, escaped: false, overlap: false });
        expect(geometry.pageWidth).toBeLessThanOrEqual(geometry.viewport);
        measurements.push({ width, scenario, ...geometry });
        await expect(trigger.locator(".execution-provider")).toHaveCount(0);
        await expect(trigger.locator("svg")).toHaveCount(0);
        await expect(
          trigger.locator(".execution-inherited-label"),
        ).toBeVisible();
        await trigger.focus();
        await page.keyboard.press("Enter");
        await expect(trigger).toHaveAttribute("aria-expanded", "true");
        const roles = page.getByRole("radiogroup", {
          name: "Agent role",
          exact: true,
        });
        await expect(roles).toBeVisible();
        for (const [role, label, selected] of [
          ["Lead", /^(Main agent|Subagent) model$/, "opus"],
          [
            "Workers",
            /^(Default subagent|Subagent) model$/,
            scenario.includes("worker=1") ? "opus" : "sol",
          ],
          ["Review", "Default review model", "__model_default__"],
        ] as const) {
          await roles.getByText(role, { exact: true }).click();
          const models = page.getByRole("listbox", {
            name: label,
            exact: true,
          });
          await expect(models).toBeVisible();
          await expect(
            models.locator(`[role="option"][data-value="${selected}"]`),
          ).toHaveAttribute("aria-selected", "true");
        }
        // Reload closes either the mobile sheet or desktop popover for the next case.
      }
    }
    if (screenshots) {
      await writeFile(
        join(screenshots, "measurements.json"),
        JSON.stringify(measurements, null, 2),
      );
      for (const width of [320, 360, 390, 1440]) {
        await page.setViewportSize({ width, height: 844 });
        for (const theme of ["light", "dark"]) {
          await page.goto(`${url}?theme=${theme}`);
          await expect(
            page.locator(".execution-summary-trigger"),
          ).toBeVisible();
          await page.screenshot({
            path: join(screenshots, `composer-${width}-${theme}.png`),
          });
        }
      }
    }
  } finally {
    await server.close();
  }
});
