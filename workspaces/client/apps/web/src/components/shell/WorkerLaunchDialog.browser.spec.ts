import { fixtureOrigin } from "../../../tests/misc-fixture.ts";
import type {} from "./WorkerLaunchDialog.browser.fixture.tsx";
import { fileURLToPath } from "node:url";
import {
  test,
  expect,
  createFixtureViteServer,
} from "../../../tests/playwright.ts";

for (const width of [320, 1200]) {
  test(`worker launch permissions at ${width}px`, async ({
    browser,
  }, testInfo) => {
    const server = await createFixtureViteServer({
      configFile: false,
      root: fileURLToPath(new URL("../../..", import.meta.url)),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "worker-launch-fixture",
          configureServer(server) {
            server.middlewares.use((request, response, next) => {
              if (request.url !== "/worker-launch-fixture") return next();
              response.setHeader("Content-Type", "text/html");
              response.end(
                '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><title>Worker launch</title><div id="root"></div><script type="module" src="/src/components/shell/WorkerLaunchDialog.browser.fixture.tsx"></script>',
              );
            });
          },
        },
      ],
    });
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    try {
      await server.listen();
      await page.goto(`${fixtureOrigin(server)}/worker-launch-fixture`);
      const dialog = page.getByRole("dialog", {
        name: "Start Research worker",
      });
      const selector = dialog.getByLabel("Permission mode");
      const task = dialog.getByLabel("Task for this subagent");
      await expect(dialog.getByText("Inherits: auto from lead")).toBeVisible();
      await expect(selector.locator("option")).toHaveText([
        "Inherit from lead",
        "default",
        "acceptEdits",
        "auto",
        "plan",
      ]);
      await task.fill("Read the project and report the result.");
      await page.screenshot({
        path: testInfo.outputPath(`worker-launch-claude-${width}.png`),
        fullPage: true,
      });
      await dialog.getByRole("button", { name: "Start subagent" }).click();
      await expect
        .poll(() => page.evaluate(() => window.workerLaunchFixture.submitted))
        .toMatchObject({ parent: "lead-id", profile_id: "profile-id" });
      expect(
        await page.evaluate(
          () =>
            window.workerLaunchFixture.submitted !== null &&
            "permission_mode" in window.workerLaunchFixture.submitted,
        ),
      ).toBe(false);
      await selector.selectOption("acceptEdits");
      await dialog.getByRole("button", { name: "Start subagent" }).click();
      await expect
        .poll(() =>
          page.evaluate(
            () => window.workerLaunchFixture.submitted?.permission_mode,
          ),
        )
        .toBe("acceptEdits");
      await page.evaluate(() =>
        window.workerLaunchFixture.launch(
          "bypassPermissions",
          "claude",
          true,
          true,
        ),
      );
      await expect(
        dialog.getByText("Lead mode: bypassPermissions"),
      ).toBeVisible();
      await expect(selector).toBeDisabled();
      await expect(selector).toHaveValue("plan");
      await task.fill("Review the project.");
      await dialog.getByRole("button", { name: "Start subagent" }).click();
      await expect
        .poll(() =>
          page.evaluate(
            () => window.workerLaunchFixture.submitted?.permission_mode,
          ),
        )
        .toBe("plan");
      await page.screenshot({
        path: testInfo.outputPath(`worker-launch-reviewer-${width}.png`),
        fullPage: true,
      });
      await page.evaluate(() =>
        window.workerLaunchFixture.launch("auto", "codex"),
      );
      await expect(
        dialog.getByText(
          "Codex sandbox: workspace-write. Approval policy: on-request. These settings follow the lead.",
        ),
      ).toBeVisible();
      await expect(selector).toHaveCount(0);
      await task.fill("Read the project.");
      await dialog.getByRole("button", { name: "Start subagent" }).click();
      expect(
        await page.evaluate(
          () =>
            window.workerLaunchFixture.submitted !== null &&
            "permission_mode" in window.workerLaunchFixture.submitted,
        ),
      ).toBe(false);
      await page.screenshot({
        path: testInfo.outputPath(`worker-launch-codex-${width}.png`),
        fullPage: true,
      });
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      ).toBe(true);
      expect(errors).toEqual([]);
    } finally {
      await page.close();
      await server.close();
    }
  });
}
