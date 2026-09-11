#!/usr/bin/env node
// Production bundle, isolated backend, no model calls.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-ux-navigation-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  log = "";
fixture.stderr.on("data", (chunk) => {
  log += chunk;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await (await fetch(origin + "/api/state")).json();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  page.setDefaultTimeout(12000);
  const errors = [],
    actions = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/action", (route) => {
    actions.push(route.request().postDataJSON());
    return route.fulfill({ json: { status: "accepted" } });
  });
  await page.goto(origin);
  await page.locator("#message").waitFor();
  await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
  await page
    .getByRole("button", { name: "Close conversations", exact: true })
    .click();
  await page.locator("#sidebar").waitFor({ state: "hidden" });
  await page.reload();
  await page.locator("#message").waitFor();
  assert.equal(await page.locator("#sidebar").count(), 0);
  await page.locator("#sidebar-toggle").click();
  await page.locator("#sidebar").waitFor();
  await page.locator("#team-toggle").click();
  await page.locator("#team").waitFor();
  await page.locator("#team-close").click();
  await page.locator("#team").waitFor({ state: "hidden" });
  await page.getByRole("button", { name: "Search chats", exact: true }).click();
  await page.getByRole("dialog").waitFor();
  await page.keyboard.press("Escape");
  await page.keyboard.press("Control+k");
  await page.getByRole("dialog").waitFor();
  await page.keyboard.press("Escape");
  const other = snapshot.threads.find((item) => item.name === "Other project");
  await page.evaluate(
    (agentId) =>
      window.dispatchEvent(
        new CustomEvent("studio-navigate", {
          detail: { agentId, section: "messages" },
        }),
      ),
    other.id,
  );
  await page.getByRole("dialog", { name: "Messages", exact: true }).waitFor();
  await page.waitForFunction(
    () =>
      document.querySelector("#conversation-title").textContent ===
      "Other project",
  );
  await page.keyboard.press("Escape");
  await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
  for (const width of [1440, 390, 320]) {
    await page.setViewportSize({ width, height: 960 });
    for (const scheme of ["light", "dark"]) {
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
      const dialog = page.getByRole("dialog", {
        name: "Chat settings",
        exact: true,
      });
      await dialog
        .getByLabel("Appearance", { exact: true })
        .selectOption(scheme);
      await page.waitForFunction(
        (scheme) =>
          document.documentElement.dataset.mantineColorScheme === scheme,
        scheme,
      );
      await page.screenshot({
        path: join(evidence, `settings-${width}-${scheme}.png`),
      });
      await page.keyboard.press("Escape");
      await page.getByRole("dialog").waitFor({ state: "hidden" });
      const geometry = await page.evaluate(() => ({
        width: innerWidth,
        overflow: document.documentElement.scrollWidth,
        controls: [
          ...document.querySelectorAll(".simple-workspace-header button"),
        ]
          .filter((node) => node.getBoundingClientRect().width)
          .map((node) => {
            const rect = node.getBoundingClientRect();
            return {
              label: node.getAttribute("aria-label") || node.textContent,
              left: rect.left,
              right: rect.right,
            };
          }),
      }));
      assert.ok(geometry.overflow <= geometry.width, JSON.stringify(geometry));
      assert.ok(
        geometry.controls.every(
          (item) => item.left >= 0 && item.right <= width + 1,
        ),
        JSON.stringify(geometry),
      );
      await page.screenshot({
        path: join(evidence, `chat-${width}-${scheme}.png`),
      });
    }
    if (width < 761) {
      await page.locator("#sidebar-toggle").click();
      await page
        .getByRole("button", { name: "Add project", exact: true })
        .click();
      await page
        .getByRole("dialog", { name: "Add project", exact: true })
        .waitFor();
      await page.keyboard.press("Escape");
      await page.keyboard.press("Escape");
    }
  }
  const firstUse = await browser.newPage({
    viewport: { width: 320, height: 844 },
    isMobile: true,
    hasTouch: true,
  });
  firstUse.on("pageerror", (error) => errors.push(error.message));
  const emptyState = {
    ...snapshot,
    stateDir: snapshot.stateDir + "/phone-first",
    threads: [],
    chats: [],
    runtime: {
      ...snapshot.runtime,
      agents: [],
      projects: [],
      rooms: [],
      requests: [],
      complaints: [],
      tasks: [],
      monitors: [],
    },
  };
  await firstUse.route("**/api/sync/identity", (route) =>
    route.fulfill({ status: 404, json: { error: "Fixture without sync" } }),
  );
  await firstUse.route("**/api/state", (route) =>
    route.fulfill({ json: emptyState }),
  );
  await firstUse.route("**/api/directories*", (route) =>
    route.fulfill({
      json: { path: "/workspace/phone-first", directories: [] },
    }),
  );
  const creations = [],
    projects = [];
  let createdResolve;
  const createdRequest = new Promise((resolve) => {
    createdResolve = resolve;
  });
  await firstUse.route("**/api/projects", (route) => {
    projects.push(route.request().postDataJSON());
    return route.fulfill({ json: {} });
  });
  await firstUse.route("**/api/leads", (route) => {
    const body = route.request().postDataJSON();
    creations.push(body);
    createdResolve();
    return route.fulfill({ json: { id: body.id } });
  });
  await firstUse.goto(origin);
  await firstUse.locator("#sidebar-toggle").click();
  await firstUse.getByRole("button", { name: "New chat", exact: true }).click();
  const folderDialog = firstUse.getByRole("dialog", {
    name: "Choose project folder",
    exact: true,
  });
  await folderDialog
    .getByRole("button", { name: "Use this folder", exact: true })
    .click();
  await createdRequest;
  assert.equal(projects.length, 1);
  assert.equal(creations.length, 1);
  assert.equal(creations[0].cwd, "/workspace/phone-first");
  assert.ok(creations[0].id);
  await firstUse.close();
  const branchPage = await browser.newPage({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
  });
  branchPage.on("pageerror", (error) => errors.push(error.message));
  const sourceAgent = {
    ...snapshot.threads.find((item) => item.name === "Other project"),
    status: "idle",
    inFlight: false,
    canSend: true,
    threadId: "fixture-source-thread",
    turnId: undefined,
  };
  let branchAgent,
    showBranch = false;
  const branchState = () => ({
    ...snapshot,
    stateDir: snapshot.stateDir + "/branch",
    threads: showBranch ? [sourceAgent, branchAgent] : [sourceAgent],
    chats: [],
    runtime: {
      ...snapshot.runtime,
      agents: showBranch ? [sourceAgent, branchAgent] : [sourceAgent],
      rooms: [],
      requests: [],
      complaints: [],
      tasks: [],
      monitors: [],
    },
  });
  await branchPage.route("**/api/sync/identity", (route) =>
    route.fulfill({ status: 404, json: { error: "Fixture without sync" } }),
  );
  await branchPage.route("**/api/state", (route) =>
    route.fulfill({ json: branchState() }),
  );
  await branchPage.route("**/api/transcript/stream*", (route) =>
    route.fulfill({ status: 503, json: { error: "Use fixture polling" } }),
  );
  await branchPage.route("**/api/transcript?*", (route) => {
    const id = new URL(route.request().url()).searchParams.get("id");
    return route.fulfill({
      json: {
        items:
          id === sourceAgent.id
            ? [
                {
                  id: "branch-source-message",
                  role: "user",
                  text: "Original branch prompt",
                  turnId: "completed-turn",
                  turnStatus: "completed",
                  at: 1,
                },
              ]
            : [],
        agent: { id, status: "idle" },
        historyVersion: "branch-fixture",
      },
    });
  });
  await branchPage.route("**/api/branch", (route) => {
    const body = route.request().postDataJSON();
    branchAgent = {
      ...sourceAgent,
      id: body.id,
      rootId: body.id,
      name: "Reviewed branch",
      threadId: "fixture-new-thread",
    };
    return route.fulfill({
      json: {
        agent: branchAgent,
        draft: { text: "Original branch prompt", assets: [] },
      },
    });
  });
  await branchPage.goto(origin);
  await branchPage.locator("#message").fill("Preserve source draft");
  await branchPage
    .getByRole("button", { name: "Edit in a new chat", exact: true })
    .click();
  await branchPage
    .getByRole("textbox", { name: "Draft for the new chat", exact: true })
    .fill("Reviewed mobile branch draft");
  await branchPage
    .getByRole("button", { name: "Create draft in new chat", exact: true })
    .click();
  await branchPage.waitForFunction(
    () =>
      document.querySelector("#message")?.value ===
      "Reviewed mobile branch draft",
  );
  await branchPage.waitForTimeout(1800);
  assert.equal(
    await branchPage.locator("#message").inputValue(),
    "Reviewed mobile branch draft",
    "new branch stays selected before the projection arrives",
  );
  showBranch = true;
  await branchPage.waitForFunction(
    () =>
      document.querySelector("#conversation-title")?.textContent ===
      "Reviewed branch",
  );
  assert.equal(
    await branchPage.evaluate(
      (id) => JSON.parse(localStorage.getItem("codex-agent-drafts"))[id],
      sourceAgent.id,
    ),
    "Preserve source draft",
  );
  await branchPage.close();
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ result: "PASS", evidence }));
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
