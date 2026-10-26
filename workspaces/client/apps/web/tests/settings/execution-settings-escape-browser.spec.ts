import { fixtureUrl } from "./fixture-helpers.ts";
import type {} from "./execution-settings-escape-browser.fixture.tsx";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "../playwright.ts";

for (const width of [1440, 390]) {
  test(`execution settings preserve nested Escape and focus at ${width}px`, async ({
    page,
  }) => {
    test.setTimeout(90000);
    test.fail(
      width === 390,
      "Account-modal Escape also closes the footer Drawer on phones.",
    );
    const root = join(import.meta.dirname, "../..");
    const { createServer } = await import("vite");
    const cacheDir = await mkdtemp(join(tmpdir(), "native-execution-escape-"));
    const server = await createServer({
      root,
      configFile: false,
      cacheDir,
      server: { host: "127.0.0.1", port: 0, hmr: false },
      plugins: [
        {
          name: "execution-escape",
          configureServer(server) {
            server.middlewares.use("/check", (_req, res) => {
              res.setHeader("Content-Type", "text/html");
              res.end(
                '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/tests/settings/execution-settings-escape-browser.fixture.tsx"></script>',
              );
            });
          },
        },
      ],
    });
    await server.listen();
    try {
      await page.setViewportSize({ width, height: 844 });
      await page.goto(fixtureUrl(server, "check"));
      const parent = page.getByRole("dialog", {
        name: "Chat settings",
        exact: true,
      });
      const trigger = parent.getByRole("button", {
        name: "Main agent settings",
        exact: true,
      });
      await trigger.click();
      const picker = page.getByRole("dialog", {
        name: "Main agent settings",
        exact: true,
      });
      await expect(picker).toBeVisible();
      await picker.getByRole("radio", { name: "Astra 6", exact: true }).focus();
      await page.keyboard.press("Escape");
      await expect(picker).not.toBeVisible();
      await expect(parent).toBeVisible();
      await expect(trigger).toBeFocused();
      await trigger.click();
      await picker.locator('[data-account-key="second"]').click();
      const account = page.getByRole("dialog", {
        name: "Change account",
        exact: true,
      });
      await expect(account).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(account).not.toBeVisible();

      await expect(picker).toBeVisible();
      await expect(parent).toBeVisible();
      await picker.getByRole("radio", { name: "Astra 6", exact: true }).focus();
      await page.keyboard.press("Escape");
      await expect(picker).not.toBeVisible();
      await expect(parent).toBeVisible();
    } finally {
      await server.close();
      await rm(cacheDir, { recursive: true, force: true });
    }
  });
}
