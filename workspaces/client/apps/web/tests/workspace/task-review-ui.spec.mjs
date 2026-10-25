import { createRequire } from "node:module";
const { test, expect } = createRequire(import.meta.url)("@playwright/test");
import { createServer } from "vite";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// Hidden, isolated renderer fixture. It sends every action through the real Tasks caller.
test("Tasks show gate evidence and schema refusals and require an override reason", async ({
  page,
}, testInfo) => {
  const root = resolve(import.meta.dirname, "../..");
  const cacheDir = await mkdtemp(join(tmpdir(), "task-review-ui-"));
  const entry = join(root, "task-review-fixture.tsx");
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0, watch: null, hmr: false },
    plugins: [
      {
        name: "task-review-fixture",
        enforce: "pre",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<meta name="viewport" content="width=device-width, initial-scale=1"><div id="root"></div><script type="module" src="/task-review-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id === "/task-review-fixture.tsx") return entry;
          if (id.endsWith("/servers/ServerServicesContext"))
            return "\0task-review-api";
          if (id.endsWith("/sync/subscriptions")) return "\0task-review-stream";
        },
        load(id) {
          if (id === "\0task-review-stream")
            return `
            import {useSyncExternalStore} from 'react';
            const subscribe=accept=>{window.addEventListener('task-review-frame',accept);return()=>window.removeEventListener('task-review-frame',accept)};
            export const useSubscription=ref=>useSyncExternalStore(subscribe,()=>window.taskReviewFrames[ref.kind]);`;
          if (id === "\0task-review-api")
            return `
          const api={
            get:async(path,options)=>{const response=await fetch(path+'?'+new URLSearchParams(options?.query));if(!response.ok)throw new Error('Read failed');return response.json();},
            post:async(path,body)=>{const response=await fetch(path,{method:'POST',body:JSON.stringify(body)});const result=await response.json();if(!response.ok)throw new Error(result.error);return result;},
            apiDownload:async()=>({blob:new Blob(['failed check']),name:'gate.log',truncated:false}),
          };
          export const useServerApi=()=>api;
          export const useServerStorage=()=>localStorage;`;
          if (id === entry)
            return `
          import React from 'react';
          import {createRoot} from 'react-dom/client';
          import {MantineProvider} from '@mantine/core';
          import '@mantine/core/styles.css';
          import Tasks from '/src/components/shell/Tasks.tsx';
          createRoot(document.getElementById('root')).render(<MantineProvider><main style={{maxWidth:1100,margin:'0 auto',padding:8}}><h1>Tasks</h1><Tasks agentId="worker" rootId="lead"/></main></MantineProvider>);`;
        },
      },
    ],
  });
  const gate = { command: "pnpm test", timeout_ms: 600000 };
  const schema = {
    type: "object",
    properties: { count: { type: "integer" } },
    required: ["count"],
  };
  const common = {
    rootId: "lead",
    created: 1,
    updated: 1,
    version: 2,
    owner: "worker",
    dependencies: [],
    decisions: [],
    results: [],
  };
  const tasks = [
    {
      ...common,
      id: "review-task",
      title: "Review the release",
      status: "review",
      description:
        "Inspect the failed check and the submitted result before acceptance.",
      gate,
      resultSchema: schema,
      submissionRefusal: {
        failedSubmits: 3,
        limit: 3,
        retryAllowed: false,
        errors: [{ path: "/count", message: '"bad" is not of type "integer"' }],
      },
      results: [
        {
          id: "result-1",
          agent: "worker",
          text: "Completed",
          checks: "Unit checks",
          revision: "a".repeat(40),
          files: ["src/task.ts"],
          created: 2,
          result: { count: 2 },
          gate: {
            status: "failed",
            config: gate,
            cwd: "/workspace",
            revision: "a".repeat(40),
            monitorId: "gate-monitor",
            exitCode: 1,
            output: "FAIL release check\nExpected 3 results, received 2",
            log: "/state/monitors/gate-monitor.log",
          },
        },
      ],
      decisions: [
        {
          decision: "reject",
          reason: "Add the missing check.",
          by: "lead",
          created: 1,
        },
      ],
    },
    {
      ...common,
      id: "ready-task",
      title: "Prepare the next result",
      status: "ready",
      description: "Next step",
      resultSchema: schema,
      gate: null,
    },
    {
      ...common,
      id: "blocked-task",
      title: "Publish after review",
      status: "ready",
      displayStatus: "blocked",
      owner: null,
      dependencies: ["ready-task"],
    },
  ];
  const writes = [];
  let taskDefaultGate = gate;
  const frames = () => ({
    agentDetail: {
      data: { id: "lead", taskDefaultGate },
      status: "live",
      error: null,
    },
    work: {
      data: { rootId: "lead", taskDefaultGate, items: tasks, tasks },
      status: "live",
      error: null,
    },
  });
  const reads = [];
  page.on("request", (request) => {
    if (
      request.method() === "GET" &&
      new URL(request.url()).pathname === "/api/work"
    )
      reads.push(request.url());
  });
  let lostAccept = true;
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.addInitScript((value) => {
      window.taskReviewFrames = value;
    }, frames());
    await page.route("**/api/work*", async (route) => {
      if (route.request().method() === "GET")
        return route.fulfill({
          json: { rootId: "lead", taskDefaultGate, items: tasks, tasks },
        });
      const body = route.request().postDataJSON();
      writes.push(body);
      if (body.action === "accept") {
        if (lostAccept) {
          lostAccept = false;
          return route.fulfill({
            status: 503,
            json: { error: "The reply was lost. Retry the same action." },
          });
        }
        tasks[0].status = "accepted";
        tasks[0].version++;
        tasks[0].decisions.push({
          decision: "accept",
          reason: body.result,
          gateOverrideReason: body.override_reason,
          by: "lead",
          created: 3,
        });
      } else if (body.action === "update") {
        if (body.result_schema?.type === "invalid")
          return route.fulfill({
            status: 400,
            json: { error: "Invalid result_schema at /type: unknown type" },
          });
        Object.assign(tasks[1], {
          title: body.title,
          description: body.description,
          resultSchema: body.result_schema,
          gate: body.gate,
          version: tasks[1].version + 1,
        });
      } else if (body.action === "set_default_gate")
        taskDefaultGate = body.gate;
      else if (body.action === "create")
        tasks.push({
          ...common,
          ...body,
          id: "created-task",
          status: body.status,
          resultSchema: body.result_schema,
        });
      else if (body.action === "reject") {
        tasks[0].status = "ready";
        tasks[0].version++;
      }
      await page.evaluate((value) => {
        window.taskReviewFrames = value;
        window.dispatchEvent(new Event("task-review-frame"));
      }, frames());
      return route.fulfill({ json: { ok: true } });
    });
    await page.setViewportSize({ width: 320, height: 800 });
    await page.goto(
      `http://127.0.0.1:${server.httpServer.address().port}/check`,
    );
    await page.getByRole("button", { name: /Review the release/ }).click();
    const detail = page.getByRole("article", { name: "Task detail" });
    await expect(detail).toContainText("Gate status: failed");
    await expect(detail).toContainText("Exit code1");
    await expect(detail.getByLabel("Gate output")).toContainText(
      "FAIL release check",
    );
    await expect(
      detail.getByRole("button", { name: "Download gate log" }),
    ).toBeVisible();
    await expect(detail).toContainText("3 of 3 attempts refused.");
    await expect(detail).toContainText("/count");
    await expect(detail.getByRole("alert")).toContainText("submission limit");
    await expect(detail.getByLabel("Latest result JSON")).toContainText(
      '"count": 2',
    );
    await expect(
      page.getByRole("button", { name: "Reject task", exact: true }),
    ).toBeDisabled();
    await page
      .getByLabel("Decision reason")
      .fill("Checked the remaining results.");
    const accept = page.getByRole("button", { name: "Accept with override" });
    await expect(accept).toBeDisabled();
    await page.getByLabel("Gate override reason").fill("   ");
    await expect(accept).toBeDisabled();
    await page.screenshot({
      path: testInfo.outputPath("task-review-320.png"),
      fullPage: true,
    });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    await page.setViewportSize({ width: 1200, height: 900 });
    await expect(accept).toBeDisabled();
    await page
      .getByLabel("Gate override reason")
      .fill(
        "The check needs the staging service. I verified the release locally.",
      );
    await expect(accept).toBeEnabled();
    await page.getByText("Result history (1)", { exact: true }).click();
    await page.getByText("Decision history (1)", { exact: true }).click();
    await expect(detail).toContainText("Add the missing check.");
    await page.screenshot({
      path: testInfo.outputPath("task-review-1200.png"),
      fullPage: true,
    });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
    ).toBe(true);
    await accept.click();
    await expect(
      page.getByRole("alert").filter({ hasText: "reply was lost" }),
    ).toBeVisible();
    await accept.click();
    await expect(detail).toContainText("Create a follow-up task");
    const accepts = writes.filter((write) => write.action === "accept");
    expect(accepts).toHaveLength(2);
    expect(accepts[1]).toEqual(accepts[0]);
    expect(accepts[0]).toMatchObject({
      agent: "lead",
      task_id: "review-task",
      version: 2,
      override_reason:
        "The check needs the staging service. I verified the release locally.",
    });
    expect(accepts[0].id).toBeTruthy();

    await page.getByLabel("Filter by status").selectOption("blocked");
    await expect(
      page.getByRole("button", { name: /Publish after review/ }),
    ).toBeVisible();
    await expect(
      page.getByRole("button", { name: /Prepare the next result/ }),
    ).toHaveCount(0);
    await page.getByLabel("Filter by status").selectOption("ready");
    await page.getByLabel("Filter by owner").selectOption("worker");
    await page.getByRole("button", { name: /Prepare the next result/ }).click();
    await page
      .getByRole("button", { name: "Update task", exact: true })
      .click();
    const editor = page.getByRole("form", { name: "Update task" });
    await editor.getByLabel("Result schema (JSON)").fill("{");
    await expect(
      editor.getByRole("button", { name: "Save task" }),
    ).toBeDisabled();
    await editor.getByLabel("Result schema (JSON)").fill('{"type":"invalid"}');
    await editor.getByRole("button", { name: "Save task" }).click();
    await expect(page.getByRole("alert")).toContainText(
      "Invalid result_schema at /type",
    );
    await editor
      .getByLabel("Result schema (JSON)")
      .fill('{"type":"object","required":["count"]}');
    await editor.getByLabel("Gate command").fill("pnpm check");
    await editor.getByLabel("Gate timeout (milliseconds)").fill("1000");
    await editor.getByRole("button", { name: "Save task" }).click();
    await expect(editor).toHaveCount(0);
    expect(writes.at(-1)).toMatchObject({
      action: "update",
      version: 2,
      gate: { command: "pnpm check", timeout_ms: 1000 },
      result_schema: { type: "object", required: ["count"] },
    });

    await page.getByText("Team default gate", { exact: true }).click();
    await page.getByLabel("Gate command", { exact: true }).fill("pnpm verify");
    await page.getByLabel("Gate timeout (milliseconds)").fill("2000");
    await page.getByRole("button", { name: "Save team default gate" }).click();
    await expect(page.getByRole("status")).toContainText("completed");
    expect(writes.at(-1)).toMatchObject({
      action: "set_default_gate",
      agent: "lead",
      gate: { command: "pnpm verify", timeout_ms: 2000 },
    });
    await page
      .getByRole("button", { name: "Create task", exact: true })
      .click();
    const create = page.getByRole("form", { name: "Create task" });
    await create.getByLabel("Title").fill("A new task");
    await create.getByLabel("Result schema (JSON)").fill("false");
    await create.getByRole("button", { name: "Save task" }).click();
    await expect(create).toHaveCount(0);
    expect(writes.at(-1)).toMatchObject({
      action: "create",
      title: "A new task",
      result_schema: false,
    });
    expect(writes.at(-1)).not.toHaveProperty("gate");
    expect(errors).toEqual([]);
    expect(reads).toEqual([]);
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
