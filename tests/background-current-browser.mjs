// Current activity lifecycle through the real component. HTTP calls use fixtures.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(root, "web/package.json"));
const { chromium } = require("playwright-core");
const { createServer } = await import(require.resolve("vite"));
const temporary = await mkdtemp(join(tmpdir(), "studio-current-background-"));
const harness = `
import React from 'react';
import {createRoot} from 'react-dom/client';
import {MantineProvider} from '@mantine/core';
import '@mantine/core/styles.css';
import '/src/style.css';
import BackgroundTasks from '/src/components/BackgroundTasks.tsx';
const root=createRoot(document.getElementById('root'));
window.renderFixture=(data,leadId='lead',initialFocus)=>root.render(
 React.createElement(MantineProvider,{defaultColorScheme:"dark"},React.createElement(BackgroundTasks,{
  opened:true,close:()=>{},data,leadId,initialFocus,
  openAgent:()=>{},refresh:async()=>{},notify:()=>{}
 })));
`;
const server = await createServer({
  configFile: false,
  root: join(root, "web"),
  cacheDir: join(temporary, "vite"),
  plugins: [
    {
      name: "current-background-fixture",
      resolveId(id) {
        if (id === "virtual:current-background") return "\0" + id;
      },
      load(id) {
        if (id === "\0virtual:current-background") return harness;
      },
    },
  ],
  server: { host: "127.0.0.1", port: 0, hmr: false },
});
await server.listen();
let browser;
try {
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 900 },
  });
  const errors = [],
    calls = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/fixture", (route) =>
    route.fulfill({
      contentType: "text/html",
      body: '<!doctype html><div id="root"></div><script type="module">import "/@id/__x00__virtual:current-background";</script>',
    }),
  );
  await page.route("**/api/**", (route) => {
    const request = route.request();
    const url = new URL(request.url());
    calls.push({
      method: request.method(),
      path: url.pathname,
      id: url.searchParams.get("id"),
    });
    return route.fulfill({
      json: { id: url.searchParams.get("id"), tail: "Live fixture output" },
    });
  });
  await page.goto(server.resolvedUrls.local[0] + "fixture");
  await page.waitForFunction(() => !!window.renderFixture);
  const agents = [
    { id: "lead", rootId: "lead", name: "Lead" },
    { id: "worker", rootId: "lead", name: "Worker" },
    { id: "other", rootId: "other", name: "Other" },
  ];
  const task = (id, status, agent = "lead") => ({
    id,
    status,
    agent,
    kind: "command",
    command: "command " + id,
    created: Date.now() / 1000 - 30,
    processId: "123",
  });
  const data = {
    token: "fixture",
    threads: agents,
    runtime: {
      requests: [
        {
          id: "approve-id",
          method: "monitor/approve",
          params: { monitorId: "approval" },
        },
      ],
      monitors: [{ ...task("approval", "approval"), kind: "monitor" }],
      tasks: [
        task("active", "running"),
        {
          ...task("tool", "running"),
          kind: "tool",
          name: "webSearch",
          command: undefined,
        },
        task("child", "running", "worker"),
        task("other-active", "running", "other"),
        ...["completed", "failed", "cancelled", "interrupted", "lost"].map(
          (status) => task(status, status),
        ),
        task("starting", "starting"),
        task("pending", "pending"),
        task("stopping", "stopping"),
      ],
    },
  };
  const render = async (leadId = "lead", focus) => {
    await page.evaluate(
      ({ data, leadId, focus }) => window.renderFixture(data, leadId, focus),
      { data, leadId, focus },
    );
  };
  const rows = () => page.locator("[data-task]");
  await render();
  await page.waitForFunction(
    () => document.querySelectorAll("[data-task]").length === 7,
  );
  assert.deepEqual(
    (
      await rows().evaluateAll((nodes) =>
        nodes.map((node) => node.dataset.task),
      )
    ).sort(),
    ["active", "approval", "child", "pending", "starting", "stopping", "tool"],
  );
  assert.equal(
    await page.getByRole("radiogroup", { name: "Task status" }).count(),
    0,
  );
  assert.equal(await page.getByText("History", { exact: true }).count(), 0);
  for (const name of ["Commands", "Monitors", "Other tools"]) {
    await page.getByRole("region", { name, exact: true }).waitFor();
    assert.equal(
      await page.getByRole("heading", { name, exact: true }).count(),
      1,
    );
  }

  assert.equal(await page.locator(".tasks-footnote").count(), 0);
  assert.equal(
    await page.getByText("Current activity", { exact: true }).count(),
    1,
  );
  const activeCommand = data.runtime.tasks.find((task) => task.id === "active");
  activeCommand.turnId = "original-turn";
  agents[0].inFlight = true;
  agents[0].turnId = "original-turn";
  await render();
  await page
    .locator('[data-task="active"] .task-row-kind')
    .getByText("Command", { exact: true })
    .waitFor();
  agents[0].turnId = "next-turn";
  await render();
  await page
    .locator('[data-task="active"] .task-row-kind')
    .getByText("Background command", { exact: true })
    .waitFor();
  agents[0].turnId = "original-turn";
  agents[0].inFlight = false;
  await render();
  await page
    .locator('[data-task="active"] .task-row-kind')
    .getByText("Background command", { exact: true })
    .waitFor();
  delete agents[0].inFlight;
  delete agents[0].turnId;
  await render();
  await page
    .locator('[data-task="active"] .task-row-kind')
    .getByText("Command", { exact: true })
    .waitFor();
  assert.equal(
    await page.locator('[data-task="approval"] .task-row-kind').innerText(),
    "Monitor",
  );
  await page.locator('[data-task="active"]').click();
  await page.getByRole("region", { name: "Command controls" }).waitFor();
  data.runtime.tasks.find((task) => task.id === "active").status = "completed";
  await render();
  await page.locator('[data-task="active"]').waitFor({ state: "detached" });
  assert.equal(
    await page.locator('[data-task-detail="active"]').count(),
    0,
    "Completion removes the selected command details",
  );
  await render("lead", {
    id: "completed",
    leadId: "lead",
    requestId: "historical-focus",
  });
  await page
    .getByText("The selected task is no longer active in this chat.", {
      exact: true,
    })
    .waitFor();
  assert.equal(await page.locator('[data-task-detail="completed"]').count(), 0);
  assert.equal(
    calls.filter((call) => call.id === "completed").length,
    0,
    "Historical focus cannot fetch old output",
  );
  await page.locator('[data-task="approval"]').click();
  await page
    .getByRole("button", { name: "Approve command", exact: true })
    .waitFor();
  await page
    .getByRole("button", { name: "Cancel monitor", exact: true })
    .waitFor();
  await page.getByLabel("Task type", { exact: true }).selectOption("command");
  assert.equal(await page.locator('[data-task="approval"]').count(), 0);
  await page.getByLabel("Task type", { exact: true }).selectOption("all");
  await page.getByLabel("Find a task", { exact: true }).fill("child");
  assert.deepEqual(
    await rows().evaluateAll((nodes) => nodes.map((node) => node.dataset.task)),
    ["child"],
  );
  await page.getByLabel("Find a task", { exact: true }).fill("");
  await render("other");
  await page.locator('[data-task="other-active"]').waitFor();
  assert.deepEqual(
    await rows().evaluateAll((nodes) => nodes.map((node) => node.dataset.task)),
    ["other-active"],
  );
  for (const width of [320, 1920]) {
    await page.setViewportSize({ width, height: 900 });
    await render("lead", {
      id: "child",
      leadId: "lead",
      requestId: "child-focus-" + width,
    });
    await page.locator('[data-task-detail="child"]').waitFor();
    await page.getByRole("region", { name: "Command controls" }).waitFor();
    assert.equal(
      await page.evaluate(
        () => document.documentElement.scrollWidth > innerWidth,
      ),
      false,
      "No page overflow at " + width,
    );
    await page.screenshot({
      path: join(tmpdir(), "studio-current-activity-" + width + ".png"),
    });
    data.runtime.tasks.find((task) => task.id === "child").status = "completed";
    await render("lead");
    await page
      .locator('[data-task-detail="child"]')
      .waitFor({ state: "detached" });
    data.runtime.tasks.find((task) => task.id === "child").status = "running";
  }
  data.runtime.tasks = data.runtime.tasks.map((task) => ({
    ...task,
    status: "completed",
  }));
  data.runtime.monitors = data.runtime.monitors.map((task) => ({
    ...task,
    status: "completed",
  }));
  await render("lead");
  await page.getByText("No active tasks", { exact: true }).waitFor();
  assert.equal(await rows().count(), 0);
  assert.equal(await page.locator("[data-task-detail]").count(), 0);
  assert.equal(
    calls.filter((call) => call.path === "/api/workspace").length,
    0,
    "Current activity does not fetch command history",
  );
  assert.equal(
    calls.filter((call) => call.method !== "GET").length,
    0,
    "Visibility changes cannot replay or cancel work",
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS: current snapshot only, completion and historical focus hide details, approval/process controls, filters, chat scope, 320/1920 viewports, no history reads or writes",
  );
} finally {
  await browser?.close();
  await server.close();
  await rm(temporary, { recursive: true, force: true });
}
