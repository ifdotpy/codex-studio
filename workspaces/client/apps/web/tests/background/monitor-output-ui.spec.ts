// Hidden browser fixture for the real activity drawer. Native processes use HTTP fixtures.
import { createRequire } from "node:module";
const { test, expect } = createRequire(import.meta.url)("@playwright/test");
import { createServer } from "vite";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

for (const width of [320, 1200]) {
  test(`monitor output controls at ${width}px`, async ({ page }, testInfo) => {
    const root = fileURLToPath(new URL("../../", import.meta.url));
    const temporary = await mkdtemp(join(tmpdir(), "studio-monitor-output-"));
    const harness = `
import React from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import '/src/style.css';
import BackgroundTasks from '/src/components/shell/BackgroundTasks.tsx';
const data={threads:[{id:'lead',rootId:'lead',name:'Lead',epoch:2,accountKey:'default'}],runtime:{tasks:[],monitors:[],requests:[]}};
createRoot(document.getElementById('root')).render(React.createElement(MantineProvider,{defaultColorScheme:'dark'},React.createElement(BackgroundTasks,{
opened:true,close:()=>{},data,leadId:'lead',initialFocus:{id:'monitor',leadId:'lead',requestId:'focus'},openAgent:()=>{},refresh:async()=>{},notify:message=>(window.notices??=[]).push(message)
})));
`;
    const server = await createServer({
      configFile: false,
      root,
      cacheDir: join(temporary, "vite"),
      plugins: [
        {
          name: "monitor-output-fixture",
          resolveId(id) {
            if (id === "virtual:monitor-output") return "\0" + id;
          },
          load(id) {
            if (id === "\0virtual:monitor-output") return harness;
          },
        },
      ],
      server: { host: "127.0.0.1", port: 0, hmr: false },
    });
    await server.listen();
    const calls = [];
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    const common = {
      agent: "lead",
      accountKey: "default",
      epoch: 2,
      status: "running",
      created: Date.now() / 1000 - 60,
      generation: "generation",
      cancelRequested: false,
      bytes: 0,
      output: "",
      outputTruncated: false,
      capabilities: {
        readOutput: true,
        sendInput: false,
        resize: false,
        stop: true,
      },
    };
    const monitor = {
      ...common,
      id: "monitor",
      key: "monitor-key",
      source: "monitor",
      name: "Build watcher",
      command: "printf 'build complete'",
      outputPatterns: ["build"],
      outputWakeBudget: {
        totalSuppressedMatches: 1250,
        paused: true,
        tokens: 0,
      },
    };
    const background = {
      ...common,
      id: "claude",
      key: "claude-key",
      source: "claude_task",
      name: "Claude background task",
      outputFile: "/workspace/logs/background-output.txt",
    };
    let restartUnknown = true;
    let outputDenied = false;
    await page.route("**/fixture", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: '<!doctype html><div id="root"></div><script type="module">import "/@id/__x00__virtual:monitor-output";</script>',
      }),
    );
    await page.route("**/api/**", async (route) => {
      const request = route.request();
      const url = new URL(request.url());
      if (request.method() === "POST") {
        calls.push({ path: url.pathname, body: request.postDataJSON() });
        if (url.pathname === "/api/monitor/restart") {
          if (restartUnknown) {
            restartUnknown = false;
            return route.abort("failed");
          }
          return route.fulfill({
            json: {
              id: "replacement",
              status: "starting",
              agent: "lead",
              created: 2,
            },
          });
        }
      }
      if (url.pathname === "/api/sync/identity")
        return route.fulfill({
          headers: {
            "X-Studio-API-Schema": request.headers()["x-studio-api-schema"],
          },
          json: {
            workspaceId: "1234567890abcdef1234567890abcdef",
            syncProtocol: 3,
          },
        });
      if (url.pathname === "/api/processes")
        return route.fulfill({
          json: { processes: [monitor, background], responseTarget: null },
        });
      if (url.pathname === "/api/task/output") {
        expect(url.searchParams.get("id")).toBe("claude");
        expect(url.searchParams.get("agent")).toBe("lead");
        return outputDenied
          ? route.fulfill({
              status: 400,
              json: { error: "The output file is outside the task workspace" },
            })
          : route.fulfill({
              json: {
                id: "claude",
                outputFile: background.outputFile,
                text: "Claude background output is visible.\n",
                truncated: true,
              },
            });
      }
      if (url.pathname === "/api/task")
        return route.fulfill({
          json: { id: "claude", outputFile: background.outputFile, tail: "" },
        });
      return route.fulfill({ json: {} });
    });
    try {
      await page.setViewportSize({ width, height: 900 });
      await page.goto(server.resolvedUrls.local[0] + "fixture");
      await expect(page.getByText("1250 matches suppressed")).toBeVisible();
      await expect(
        page.getByText("Output wakes paused", { exact: true }),
      ).toBeVisible();
      await expect(
        page.getByText(
          "The command is still running. Its exit will still wake the agent.",
        ),
      ).toBeVisible();
      await page.screenshot({
        path: testInfo.outputPath(`monitor-paused-${width}.png`),
        fullPage: true,
      });
      await page
        .getByRole("button", {
          name: "Restart with narrower patterns",
          exact: true,
        })
        .click();
      await expect(page.getByLabel("Command", { exact: true })).toHaveValue(
        monitor.command,
      );
      await expect(
        page.getByLabel("Output patterns, one per line"),
      ).toHaveValue("build");
      await expect(
        page.getByRole("button", { name: "Confirm restart" }),
      ).toBeDisabled();
      expect(calls).toEqual([]);
      await page.getByRole("button", { name: "Cancel", exact: true }).click();
      expect(calls).toEqual([]);
      await page
        .getByRole("button", {
          name: "Restart with narrower patterns",
          exact: true,
        })
        .click();
      await page
        .getByLabel("Output patterns, one per line")
        .fill("build complete");
      await page.screenshot({
        path: testInfo.outputPath(`monitor-confirm-${width}.png`),
        fullPage: true,
      });
      await page.getByRole("button", { name: "Confirm restart" }).click();
      await expect(
        page.getByText(
          "The restart result is unknown. Check the same request before another restart.",
        ),
      ).toBeVisible();
      await page.getByRole("button", { name: "Check restart result" }).click();
      await expect.poll(() => calls.length).toBe(2);
      expect(calls[0]).toEqual(calls[1]);
      expect(calls[0].body.outputPatterns).toEqual(["build complete"]);
      expect(calls[0].path).toBe("/api/monitor/restart");
      if (width === 320)
        await page
          .getByRole("button", { name: "Activity", exact: true })
          .click();
      await page.locator('[data-task="claude"]').click();

      const outputResponse = page.waitForResponse(
        (response) => response.url().includes("/api/task/output"),
        { timeout: 5000 },
      );
      await page
        .getByRole("button", { name: "View output", exact: true })
        .click();
      expect((await (await outputResponse).json()).text).toContain(
        "Claude background output",
      );
      await expect(
        page.getByText("Claude background output is visible.", { exact: true }),
      ).toBeVisible();
      await expect(
        page.getByText(
          "The output file exceeds the preview limit. Only its first part appears below.",
        ),
      ).toBeVisible();
      await page.screenshot({
        path: testInfo.outputPath(`claude-output-${width}.png`),
        fullPage: true,
      });
      outputDenied = true;
      await page
        .getByRole("button", { name: "View output", exact: true })
        .click();
      await expect(page.getByRole("alert")).toContainText(
        "outside the task workspace",
      );
      expect(errors).toEqual([]);
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      ).toBe(true);
    } finally {
      await server.close();
      await rm(temporary, { recursive: true, force: true });
    }
  });
}
