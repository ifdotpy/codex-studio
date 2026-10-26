// Isolated component fixture. All API requests use local test responses.
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  createFixtureViteServer,
  expect,
  handleEntitySyncFixtureRequest,
  syncIdentityFixture,
  test,
} from "../playwright.ts";

test("ux support ui", async ({ page: runnerPage }) => {
  const server = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../../", import.meta.url)),
    optimizeDeps: {
      noDiscovery: true,
      include: ["@xterm/xterm", "@xterm/addon-fit"],
    },
    server: { host: "127.0.0.1", port: 0 },
  });
  const syncFixture = {
    snapshot: { runtime: {}, stateDir: "fixture", chats: [], edges: [] },
    workspaceId: syncIdentityFixture().workspaceId,
  };
  server.middlewares.stack.unshift({
    route: "",
    handle(request, response, next) {
      if (handleEntitySyncFixtureRequest(request, response, syncFixture))
        return;
      if (new URL(request.url, "http://fixture").pathname !== "/")
        return next();
      response.setHeader("Content-Type", "text/html");
      response.end(
        '<html><div id="root"></div><script type="module" src="/tests/fixtures/ux-support.tsx"></script></html>',
      );
    },
  });
  let page;
  try {
    await server.listen();
    page = runnerPage;
    await page.setViewportSize({ width: 1100, height: 800 });
    const writes = [];
    const taskSummary = (id, created, agent = "lead") => ({
      id,
      created,
      agent,
      kind: "command",
      name: "commandExecution",
      command: id,
      status: "running",
    });
    let workspaceTasks = {};
    await page.exposeFunction("setWorkspaceTaskFixture", (stage) => {
      workspaceTasks = {
        initial: { lead: [taskSummary("first", 10)] },
        newer: {
          lead: [taskSummary("new", 20), taskSummary("first", 10)],
        },
        scope: { other: [taskSummary("other-task", 30, "other")] },
      }[stage];
    });
    const shell = {
      id: "fixture-shell",
      agent: "lead",
      title: "Fixture shell",
      cwd: "/fixture",
      status: "running",
      created: 1,
    };
    await page.addInitScript(() => {
      localStorage.setItem("codex.terminal.open", "true");
      localStorage.setItem(
        "codex.terminal.selected",
        JSON.stringify("shell:fixture-shell"),
      );
    });
    await page.route("**/api/**", async (route) => {
      const request = route.request();
      const path = new URL(request.url()).pathname;
      if (path.startsWith("/api/sync/")) return route.continue();
      if (request.method() === "POST")
        writes.push({ path, body: request.postDataJSON() });
      if (path === "/api/processes")
        return route.fulfill({ json: { processes: [], responseTarget: null } });
      if (path === "/api/workspace/tasks") {
        const agent = new URL(request.url()).searchParams.get("agent");
        const tasks = workspaceTasks[agent] || [];
        const latest = tasks.reduce(
          (result, task) => (task.created > result.created ? task : result),
          { id: "", created: 0 },
        );
        return route.fulfill({
          json: {
            tasks,
            cursor: { updated: latest.created, id: latest.id },
            hasMore: false,
            hasMoreChanges: false,
            nextBefore: null,
            reset: false,
          },
        });
      }
      if (path === "/api/task") {
        const id = new URL(request.url()).searchParams.get("id");
        const summary = Object.values(workspaceTasks)
          .flat()
          .find((task) => task.id === id);
        return route.fulfill({
          json: {
            ...summary,
            arguments: JSON.stringify({ fixture: true }),
            tail: `${id} output`,
          },
        });
      }
      const result =
        path === "/api/terminals"
          ? { items: [shell] }
          : path === "/api/terminals/output"
            ? { text: "", offset: 0, status: shell.status }
            : path === "/api/workspace"
              ? { tasks: [], monitors: [] }
              : { ok: true };
      if (path === "/api/terminals/close") shell.status = "closed";
      await route.fulfill({ json: result });
    });
    await page.clock.install();
    await page.goto(`http://127.0.0.1:${server.httpServer.address().port}`);
    await page
      .getByRole("button", { name: "End session", exact: true })
      .waitFor();
    await page.evaluate(() =>
      document.documentElement.setAttribute(
        "data-mantine-color-scheme",
        "light",
      ),
    );
    assert.deepEqual(
      await page.locator(".terminal-dock").evaluate((element) => ({
        color: getComputedStyle(element).color,
        background: getComputedStyle(element).backgroundColor,
      })),
      { color: "rgb(24, 32, 42)", background: "rgb(255, 255, 255)" },
    );
    await page.getByRole("button", { name: "Hide panel", exact: true }).click();
    assert.equal(
      writes.filter((write) => write.path.endsWith("/close")).length,
      0,
    );
    await page
      .getByRole("button", { name: "Show terminals", exact: true })
      .click();
    await page
      .getByRole("button", { name: "End session", exact: true })
      .click();
    await page
      .getByRole("alertdialog", { name: "End terminal session" })
      .waitFor();
    assert.equal(
      writes.filter((write) => write.path.endsWith("/close")).length,
      0,
    );
    await page
      .getByRole("button", { name: "Keep session", exact: true })
      .click();
    await page
      .getByRole("button", { name: "End session", exact: true })
      .click();
    await page
      .getByRole("button", {
        name: "End session and stop processes",
        exact: true,
      })
      .click();
    await page.waitForFunction(
      () => !document.querySelector('[aria-label="End terminal session"]'),
    );
    assert.deepEqual(
      writes.filter((write) => write.path.endsWith("/close")),
      [{ path: "/api/terminals/close", body: { id: "fixture-shell" } }],
    );
    await page.getByRole("button", { name: "Hide panel", exact: true }).click();
    await page.evaluate(() => window.setWorkspaceTaskFixture("initial"));
    await page.getByRole("button", { name: "Tasks", exact: true }).click();
    const support = async (name) => {
      await page.evaluate((method) => {
        const update = window.support[method];
        window.support[method] = async (...args) => {
          await window.setWorkspaceTaskFixture(method);
          return update(...args);
        };
      }, name);
      await page.evaluate((method) => window.support[method](), name);
    };
    await support("initial");
    await page.locator('[data-task-detail="first"]').waitFor();
    await expect(page.locator('[data-task="first"]')).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await support("newer");
    await page.locator('[data-task="new"]').waitFor();
    assert.equal(await page.locator('[data-task-detail="first"]').count(), 1);
    await page.clock.runFor(5200);
    assert.equal(await page.locator('[data-task-detail="first"]').count(), 1);
    await support("scope");
    await page.locator('[data-task-detail="other-task"]').waitFor();
    assert.equal(await page.locator('[data-task-detail="first"]').count(), 0);
    await page.keyboard.press("Escape");
    await page.getByRole("button", { name: "Other drafts (2)" }).click();
    await page
      .getByText("Device ID: recorded-device", { exact: true })
      .waitFor();
    assert.equal(
      await page
        .getByText("Device ID: recorded-device", { exact: true })
        .count(),
      1,
    );
    await page
      .getByRole("button", { name: "Add to text", exact: true })
      .first()
      .click();
    assert.equal(
      await page.getByRole("textbox", { name: "Draft text" }).inputValue(),
      "Current\n\nSaved text",
    );
    await page.getByRole("button", { name: "Other drafts (2)" }).click();
    await page
      .getByRole("button", { name: "Replace text", exact: true })
      .first()
      .click();
    assert.equal(
      await page.getByRole("textbox", { name: "Draft text" }).inputValue(),
      "Saved text",
    );
    console.log(
      "PASS: terminal light theme and hide/end confirmation; background selection after first load, refresh, new tasks and scope; draft metadata and explicit actions.",
    );
  } finally {
    await page?.close();
    await server.close();
  }
});
