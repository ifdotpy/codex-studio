import { createFixtureViteServer as createServer } from "../playwright.ts";
import { fixtureOrigin } from "../misc-fixture.ts";
import type {} from "./native-model-selection-browser.fixture.tsx";
import { mkdtemp, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "../playwright.ts";

// No backend: exercise the real picker with a small, deterministic catalog.
test("native model group has one Tab stop and ArrowUp wraps from the selected model", async ({
  page,
}) => {
  test.setTimeout(120000);
  const root = join(import.meta.dirname, "../..");

  const cacheDir = await mkdtemp(join(tmpdir(), "native-model-picker-"));
  const server = await createServer({
    root,
    configFile: false,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, hmr: false },
    plugins: [
      {
        name: "native-model-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/tests/models/native-model-selection-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  await server.listen();
  try {
    const url = fixtureOrigin(server) + "/check";
    await page.goto(url);
    const resolved = page.getByRole("combobox", {
      name: "Resolved Claude model",
      exact: true,
    });
    await expect(resolved).toHaveValue("opus");
    await expect(resolved.locator("option:checked")).toBeEnabled();
    await expect(
      page.getByText("Available Opus model", { exact: true }),
    ).toBeVisible();
    await resolved.selectOption("default");
    expect(
      await page.evaluate(() => window.nativeModelFixture.compactAlias),
    ).toBe("default");

    await expect(page.locator("#model")).toBeVisible();
    if (process.env.NATIVE_MODEL_SCREENSHOTS) {
      const shots = join(tmpdir(), "native-model-ui");
      await mkdir(shots, { recursive: true });
      for (const width of [320, 390, 768, 1440]) {
        await page.setViewportSize({ width, height: 568 });
        for (const theme of ["light", "dark"]) {
          await page.goto(`${url}?theme=${theme}`);
          await expect(page.locator("#model")).toBeVisible();
          await page.screenshot({
            path: join(
              shots,
              `${process.env.NATIVE_MODEL_SCREENSHOTS}-picker-${width}-${theme}.png`,
            ),
            fullPage: true,
          });
        }
      }
    }
    await page.locator("#before").focus();
    await page.keyboard.press("Tab");
    await page.keyboard.press("ArrowUp");
    await expect(page.locator('#model [data-value="opus"]')).toBeFocused();
    await expect(page.locator("#model")).toHaveAttribute("data-value", "opus");
    await page.keyboard.press("Tab");
    await expect(page.locator("#after")).toBeFocused();
    await page.keyboard.press("Shift+Tab");
    await expect(page.locator('#model [data-value="opus"]')).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(page.locator('#model [data-value="sol"]')).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(page.locator('#model [data-value="astra"]')).toBeFocused();
    await expect(page.locator("#model")).toHaveAttribute("data-value", "astra");
    const compact = page.getByRole("combobox", { name: "Compact model" });
    await expect(compact.locator("optgroup")).toHaveCount(2);
    await expect(compact.locator('optgroup[label="Claude"] option')).toHaveText(
      "Opus 4.6",
    );
    await expect(compact.locator('option[value="unavailable"]')).toBeDisabled();
    await expect(compact.locator('option[value="astra"]')).toHaveText(
      "Astra 6 (default, current)",
    );
    await compact.selectOption("__model_default__");
    await expect(compact).toHaveValue("__model_default__");
    await compact.selectOption("opus");
    await expect(
      page.getByText("Careful reasoning", { exact: true }),
    ).toBeVisible();
    await page.locator('#model [data-value="sol"]').click();
    await page.locator('#model [data-value="sol"]').focus();
    // Home/End follow the browser's native radio behavior; do not emulate it.
    await page.keyboard.press("Home");
    await page.keyboard.press("End");
    await expect(page.locator('#model [data-value="sol"]')).toBeFocused();
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
