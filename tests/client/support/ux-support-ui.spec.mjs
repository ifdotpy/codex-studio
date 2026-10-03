// Isolated component fixture. All API requests use local test responses.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname, extname } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { expect, test } from "../playwright.mjs";

test("ux support ui", async ({ page: runnerPage }) => {
  const repo = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const require = createRequire(join(repo, "web/package.json"));
  const { build } = await import(require.resolve("vite"));
  const temporary = await mkdtemp(join(tmpdir(), "studio-support-ux-"));
  const entry = join(temporary, "index.html");
  await writeFile(
    entry,
    `<html><div id="root"></div><script type="module" src="${join(repo, "tests/fixtures/ux-support.tsx")}"></script></html>`,
  );
  let server;
  try {
    await build({
      configFile: false,
      root: temporary,
      logLevel: "error",
      resolve: {
        alias: [
          { find: /^react$/, replacement: require.resolve("react") },
          {
            find: /^react\/jsx-runtime$/,
            replacement: require.resolve("react/jsx-runtime"),
          },
          {
            find: /^react\/jsx-dev-runtime$/,
            replacement: require.resolve("react/jsx-dev-runtime"),
          },
          {
            find: /^react-dom\/client$/,
            replacement: require.resolve("react-dom/client"),
          },
          {
            find: /^@mantine\/core$/,
            replacement: require.resolve("@mantine/core"),
          },
          {
            find: /^@mantine\/core\/styles.css$/,
            replacement: require.resolve("@mantine/core/styles.css"),
          },
        ],
      },
      build: {
        outDir: join(temporary, "dist"),
        emptyOutDir: true,
        rollupOptions: { input: entry },
      },
    });
    server = createServer(async (request, response) => {
      try {
        const path = new URL(request.url, "http://fixture").pathname;
        const file = join(
          temporary,
          "dist",
          path === "/" ? "index.html" : path,
        );
        response.setHeader(
          "Content-Type",
          {
            ".html": "text/html",
            ".js": "text/javascript",
            ".css": "text/css",
          }[extname(file)] || "text/plain",
        );
        response.end(await readFile(file));
      } catch {
        response.statusCode = 404;
        response.end();
      }
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const page = runnerPage;
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
      if (request.method() === "POST")
        writes.push({ path, body: request.postDataJSON() });
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
    await page.goto(`http://127.0.0.1:${server.address().port}`);
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
      { color: "rgb(34, 39, 55)", background: "rgb(255, 255, 255)" },
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
    await page.waitForTimeout(5200);
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
    if (server) await new Promise((resolve) => server.close(resolve));
    await rm(temporary, { recursive: true, force: true });
  }
});
