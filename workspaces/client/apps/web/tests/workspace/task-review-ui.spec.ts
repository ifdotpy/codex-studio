import type { TaskReviewFrames } from "./task-review-stream.ts";
import type { components } from "../../src/generated/api.ts";
import type { PostBody } from "../../src/apiTypes.ts";
import { required, fixturePostBody } from "../teams/helpers.ts";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// Hidden, isolated renderer fixture. It sends every action through the real Tasks caller.
test("Tasks show gate evidence and schema refusals and require an override reason", async ({
  page,
}, testInfo) => {
  const root = resolve(import.meta.dirname, "../..");
  const cacheDir = await mkdtemp(join(tmpdir(), "task-review-ui-"));
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
              '<meta name="viewport" content="width=device-width, initial-scale=1"><div id="root"></div><script type="module" src="/tests/workspace/task-review-fixture.tsx"></script>',
            );
          });
        },
        resolveId(id) {
          if (id.endsWith("/servers/ServerServicesContext"))
            return fileURLToPath(
              new URL("./task-review-api.ts", import.meta.url),
            );
          if (id.endsWith("/sync/subscriptions"))
            return fileURLToPath(
              new URL("./task-review-stream.ts", import.meta.url),
            );
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
  const tasks: components["schemas"]["WorkItem"][] = [
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
  const writes: PostBody<"/api/work">[] = [];
  let taskDefaultGate: components["schemas"]["TaskAcceptanceGate"] = gate;
  const frames = (): TaskReviewFrames => ({
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
  const reads: string[] = [];
  page.on("request", (request) => {
    if (
      request.method() === "GET" &&
      new URL(request.url()).pathname === "/api/work"
    )
      reads.push(request.url());
  });
  let lostAccept = true;
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await server.listen();
    await page.addInitScript((json) => {
      const value: TaskReviewFrames = JSON.parse(json);
      window.taskReviewFrames = value;
    }, JSON.stringify(frames()));
    await page.route("**/api/work*", async (route) => {
      if (route.request().method() === "GET")
        return route.fulfill({
          json: { rootId: "lead", taskDefaultGate, items: tasks, tasks },
        });
      const body = fixturePostBody(route, "/api/work");
      writes.push(body);
      if (body.action === "accept") {
        if (lostAccept) {
          lostAccept = false;
          return route.fulfill({
            status: 503,
            json: { error: "The reply was lost. Retry the same action." },
          });
        }
        required(tasks[0]).status = "accepted";
        required(tasks[0]).version++;
        required(required(tasks[0]).decisions).push({
          decision: "accept",
          reason:
            typeof body.result === "string"
              ? body.result
              : JSON.stringify(body.result),
          gateOverrideReason: body.override_reason,
          by: "lead",
          created: 3,
        });
      } else if (body.action === "update") {
        if (
          typeof body.result_schema === "object" &&
          body.result_schema?.type === "invalid"
        )
          return route.fulfill({
            status: 400,
            json: { error: "Invalid result_schema at /type: unknown type" },
          });
        Object.assign(required(tasks[1]), {
          title: body.title,
          description: body.description,
          resultSchema: fixtureResultSchema(body.result_schema),
          gate: body.gate,
          version: required(tasks[1]).version + 1,
        });
      } else if (body.action === "set_default_gate") {
        if (body.gate === undefined) throw new Error("Missing default gate");
        taskDefaultGate = body.gate;
      } else if (body.action === "create") {
        const resultSchema = fixtureResultSchema(body.result_schema);
        tasks.push({
          ...common,
          ...body,
          id: "created-task",
          title: required(body.title),
          description: body.description ?? undefined,
          dependencies: body.dependencies ?? undefined,
          version: body.version ?? common.version,
          status: required(body.status),
          result_schema: resultSchema,
          resultSchema,
        } satisfies components["schemas"]["WorkItem"]);
      } else if (body.action === "reject") {
        required(tasks[0]).status = "ready";
        required(tasks[0]).version++;
      }
      await page.evaluate((json) => {
        const value: TaskReviewFrames = JSON.parse(json);
        window.taskReviewFrames = value;
        window.dispatchEvent(new Event("task-review-frame"));
      }, JSON.stringify(frames()));
      return route.fulfill({ json: { ok: true } });
    });
    await page.setViewportSize({ width: 320, height: 800 });
    await page.goto(required(server.resolvedUrls?.local[0]) + "check");
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
    expect(required(accepts[0]).id).toBeTruthy();

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

/** The renderer serialized this request schema as JSON before the fixture received it. */
function fixtureResultSchema(
  schema: PostBody<"/api/work">["result_schema"],
):
  | (components["schemas"]["WorkItem"]["resultSchema"] &
      components["schemas"]["JsonValue"])
  | undefined {
  return schema === undefined ? undefined : JSON.parse(JSON.stringify(schema));
}
