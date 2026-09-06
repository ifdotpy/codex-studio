#!/usr/bin/env node
// Production client with an isolated runtime. No model service or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium, _electron } = createRequire(join(skill, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-project-tree-ui-"));
const models = [
  {
    model: "gpt-6-astra",
    displayName: "Astra",
    levels: ["low", "medium", "high", "ultra"],
  },
  {
    model: "gpt-5.6-sol",
    displayName: "Sol",
    levels: ["low", "medium", "high", "ultra"],
  },
  {
    model: "gpt-5.6-luna",
    displayName: "Luna",
    levels: ["low", "medium", "high"],
  },
  { model: "test-slow", displayName: "Standard only", levels: ["low"] },
].map(({ levels, ...row }) => ({
  ...row,
  defaultReasoningEffort: "low",
  supportedReasoningEfforts: levels.map((reasoningEffort) => ({
    reasoningEffort,
  })),
  serviceTiers:
    row.model === "test-slow"
      ? []
      : [
          {
            id: "priority",
            name: "Fast",
            description: "Faster responses, increased usage",
          },
        ],
}));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, EXECUTION_SETTINGS_CATALOG: JSON.stringify(models) },
  },
);
let browser,
  log = "";
proc.stderr.on("data", (d) => (log += d));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const url = `http://127.0.0.1:${port}`;
  let page;
  if (process.env.CODEX_TEST_DESKTOP) {
    browser = await _electron.launch({
      executablePath: process.env.CODEX_TEST_DESKTOP,
      args: ["--hidden"],
      env: {
        ...process.env,
        CODEX_AGENTS_STATE_DIR: root,
        CODEX_DESKTOP_PORT: String(port),
        CODEX_DESKTOP_PROFILE: join(root, "desktop-profile"),
      },
    });
    page = await browser.firstWindow();
    assert.equal(
      await browser.evaluate(({ BrowserWindow }) =>
        BrowserWindow.getAllWindows()[0].isVisible(),
      ),
      false,
    );
  } else {
    browser = await chromium.launch({
      headless: true,
      executablePath:
        process.env.CHROME_BIN ||
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    });
    page = await browser.newPage({ viewport: { width: 1440, height: 960 } });
  }
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  if (process.env.CODEX_TEST_DESKTOP) {
    await page.waitForURL(url + "/");
    await page.locator("#message").waitFor();
  } else {
    await page.goto(url);
  }
  const state = async () =>
    (await (await fetch(url + "/api/state")).json()).runtime.agents;
  const waitFor = async (predicate) => {
    for (let i = 0; i < 100; i++) {
      const result = await predicate();
      if (result) return result;
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw Error("Timed out: " + log);
  };
  const token = (await (await fetch(url + "/api/state")).json()).token;
  const post = async (path, body) => {
    const response = await fetch(url + path, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": token,
        Origin: url,
      },
      body: JSON.stringify(body),
    });
    const data = await response.json();
    assert.ok(response.ok, JSON.stringify(data));
    return data;
  };
  const folders = {};
  for (const name of ["assistant", "litos", "Newcrom Case", "Empty project"]) {
    const path = join(root, name);
    await mkdir(path);
    folders[name] = await realpath(path);
  }
  for (const name of ["assistant", "litos", "Newcrom Case"])
    await post("/api/projects", { path: folders[name] });
  const created = [];
  for (let i = 0; i < 7; i++) {
    const chat = await post("/api/leads", {
      id: crypto.randomUUID(),
      cwd: folders.assistant,
    });
    await post("/api/rename", { id: chat.id, name: "Review component " + i });
    created.push(chat);
  }
  const legal = await post("/api/leads", {
    id: crypto.randomUUID(),
    cwd: folders["Newcrom Case"],
  });
  await post("/api/rename", { id: legal.id, name: "Review evidence" });
  await page.reload();
  const group = (name) =>
    page
      .locator(".sidebar-project")
      .filter({ has: page.locator(".project-tree-toggle", { hasText: name }) });
  await group("assistant").waitFor();
  assert.equal(await group("assistant").locator("[data-chat]").count(), 5);
  await group("assistant").getByText("Show more", { exact: true }).click();
  assert.equal(await group("assistant").locator("[data-chat]").count(), 7);
  assert.equal(
    await group("litos").getByText("No chats", { exact: true }).count(),
    1,
  );
  await group("assistant").locator(".project-tree-toggle").click();
  assert.equal(await group("assistant").locator("[data-chat]").count(), 0);
  await page.reload();
  await group("assistant").waitFor();
  assert.equal(await group("assistant").locator("[data-chat]").count(), 0);
  await page.getByLabel("Search chats").fill("Review component 2");
  await page.locator(`[data-chat="${created[2].id}"]`).click();
  await page
    .getByRole("button", { name: "New chat in litos", exact: true })
    .click();
  await waitFor(
    async () =>
      (await state()).find((a) => a.id === created[2].id).cwd === folders.litos,
  );
  assert.equal(
    (await state()).filter((a) => a.isLead).length,
    10,
    "Current empty chat is reused",
  );
  await page.getByRole("button", { name: "Add project", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Add project", exact: true });
  await dialog.getByLabel("Folder path").fill(folders["Empty project"]);
  await dialog.getByRole("button", { name: "Go", exact: true }).click();
  await dialog
    .getByRole("button", { name: "Use this folder", exact: true })
    .click();
  await dialog.waitFor({ state: "hidden" });
  await group("Empty project").getByText("No chats", { exact: true }).waitFor();
  await page.reload();
  await group("Empty project").waitFor();
  await page.screenshot({
    path: join(root, "projects-desktop.png"),
    animations: "disabled",
  });
  for (const width of [390, 320]) {
    await page.setViewportSize({ width, height: 844 });
    await page.locator("#sidebar-toggle").click();
    await page.locator("#sidebar").waitFor();
    await page.waitForFunction(() => {
      const r = document.querySelector("#sidebar").getBoundingClientRect();
      return r.x >= -1 && r.right <= innerWidth + 1;
    });
    assert.ok(
      await page
        .locator("#sidebar")
        .evaluate((el) => el.scrollWidth <= el.clientWidth),
    );
    await page.screenshot({
      path: join(root, `projects-${width}.png`),
      animations: "disabled",
    });
    await page.keyboard.press("Escape");
  }
  assert.deepEqual(errors, []);
  console.log(
    "PASS project tree: folder groups, nested chats, show more, collapse persistence, search, empty chat reuse in selected folder, add empty project persistence, mobile widths. Evidence " +
      root,
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
