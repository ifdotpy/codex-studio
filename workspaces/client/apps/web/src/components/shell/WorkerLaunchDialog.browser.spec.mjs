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
    const entry = `
      import React from 'react';
      import {createRoot} from 'react-dom/client';
      import {MantineProvider} from '@mantine/core';
      import {WorkerLaunchDialog} from '/src/components/shell/WorkerLaunchDialog.tsx';
      import '@mantine/core/styles.css';
      import '/src/components/shell/Workspace.css';
      const root = createRoot(document.getElementById('root'));
      window.launch = (mode='auto',provider='claude',reviewer=false,yolo=false) => {
        window.submitted = null;
        root.render(React.createElement(MantineProvider,null,React.createElement(WorkerLaunchDialog,{
          key: mode+provider+reviewer+yolo,
          launch:{id:'worker-id',profile_id:'profile-id',name:'Research worker',provider,reviewer},
          lead:{id:'lead-id',name:'Lead',cwd:'/fixture',provider:'claude',yoloMode:yolo,claudeOptions:{permissionMode:mode}},
          busy:false,onClose:()=>{},onStart:async body=>{window.submitted=body}
        })));
      };
      window.launch();
    `;
    const server = await createFixtureViteServer({
      configFile: false,
      root: fileURLToPath(new URL("../../..", import.meta.url)),
      server: { host: "127.0.0.1", port: 0 },
      plugins: [
        {
          name: "worker-launch-fixture",
          resolveId(id) {
            if (id === "/worker-launch-entry") return "\0worker-launch-entry";
          },
          load(id) {
            if (id === "\0worker-launch-entry") return entry;
          },
          configureServer(server) {
            server.middlewares.use((request, response, next) => {
              if (request.url !== "/worker-launch-fixture") return next();
              response.setHeader("Content-Type", "text/html");
              response.end(
                '<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1"><title>Worker launch</title><div id="root"></div><script type="module" src="/worker-launch-entry"></script>',
              );
            });
          },
        },
      ],
    });
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    try {
      await server.listen();
      await page.goto(
        `http://127.0.0.1:${server.httpServer.address().port}/worker-launch-fixture`,
      );
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
        .poll(() => page.evaluate(() => window.submitted))
        .toMatchObject({ parent: "lead-id", profile_id: "profile-id" });
      expect(
        await page.evaluate(() => "permission_mode" in window.submitted),
      ).toBe(false);
      await selector.selectOption("acceptEdits");
      await dialog.getByRole("button", { name: "Start subagent" }).click();
      await expect
        .poll(() => page.evaluate(() => window.submitted?.permission_mode))
        .toBe("acceptEdits");
      await page.evaluate(() =>
        window.launch("bypassPermissions", "claude", true, true),
      );
      await expect(
        dialog.getByText("Lead mode: bypassPermissions"),
      ).toBeVisible();
      await expect(selector).toBeDisabled();
      await expect(selector).toHaveValue("plan");
      await task.fill("Review the project.");
      await dialog.getByRole("button", { name: "Start subagent" }).click();
      await expect
        .poll(() => page.evaluate(() => window.submitted?.permission_mode))
        .toBe("plan");
      await page.screenshot({
        path: testInfo.outputPath(`worker-launch-reviewer-${width}.png`),
        fullPage: true,
      });
      await page.evaluate(() => window.launch("auto", "codex"));
      await expect(
        dialog.getByText(
          "Codex sandbox: workspace-write. Approval policy: on-request. These settings follow the lead.",
        ),
      ).toBeVisible();
      await expect(selector).toHaveCount(0);
      await task.fill("Read the project.");
      await dialog.getByRole("button", { name: "Start subagent" }).click();
      expect(
        await page.evaluate(() => "permission_mode" in window.submitted),
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
