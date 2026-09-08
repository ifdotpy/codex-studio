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
  const projectOrder = () =>
    page
      .locator(".sidebar-project")
      .evaluateAll((rows) => rows.map((row) => row.dataset.projectPath));
  const chatOrder = () =>
    group("assistant")
      .locator("[data-chat]")
      .evaluateAll((rows) => rows.map((row) => row.dataset.chat));
  const beforeProjects = await projectOrder();
  await group("litos")
    .locator(".project-tree-toggle")
    .dragTo(group("assistant").locator(".project-tree-toggle"), {
      targetPosition: { x: 45, y: 3 },
    });
  const movedProjects = await projectOrder();
  assert.ok(
    movedProjects.indexOf(folders.litos) <
      movedProjects.indexOf(folders.assistant),
  );
  assert.notDeepEqual(movedProjects, beforeProjects);
  const beforeChats = await chatOrder();
  const chatButton = (id) => page.locator(`[data-chat="${id}"]`);
  await chatButton(beforeChats.at(-1)).dragTo(chatButton(beforeChats[0]), {
    targetPosition: { x: 45, y: 3 },
  });
  assert.equal(
    (await chatOrder())[0],
    beforeChats.at(-1),
    "Chat drag persists visual order",
  );
  const pinnedId = beforeChats[3];
  const pinnedName = (await state()).find((a) => a.id === pinnedId).name;
  await chatButton(pinnedId).hover();
  await page
    .getByRole("button", { name: `Pin ${pinnedName}`, exact: true })
    .click();
  await waitFor(
    async () => (await state()).find((a) => a.id === pinnedId).pinned,
  );
  await page.waitForFunction(
    (id) =>
      document.querySelector(
        `.sidebar-project[data-project-path="${CSS.escape(id.path)}"] [data-chat]`,
      )?.dataset.chat === id.chat,
    { path: folders.assistant, chat: pinnedId },
  );
  assert.equal(
    (await chatOrder())[0],
    pinnedId,
    "Pins precede manually ordered chats",
  );
  const row = chatButton(pinnedId).locator("..");
  await page.locator("#chat-search").focus();
  await page.mouse.move(1100, 100);
  const resting = await row.locator(".row-copy").boundingBox();
  assert.equal(
    await row
      .locator(".row-actions")
      .evaluate((el) => getComputedStyle(el).opacity),
    "0",
  );
  await chatButton(pinnedId).hover();
  const hovering = await row.locator(".row-copy").boundingBox();
  assert.ok(
    resting.width >= hovering.width + 40,
    "Hidden controls return title space",
  );
  await page.mouse.move(1100, 100);
  assert.equal(
    await group("assistant")
      .locator(".project-tree-action")
      .evaluate((el) => getComputedStyle(el).opacity),
    "0",
    "Expanded projects hide actions without hover",
  );
  await page.screenshot({
    path: join(root, "projects-pinned.png"),
    animations: "disabled",
  });
  const beforeKeyboard = await chatOrder();
  const keyboardId = beforeKeyboard[2];
  await chatButton(keyboardId).focus();
  await chatButton(keyboardId).press("Alt+ArrowUp");
  assert.equal(
    (await chatOrder())[1],
    keyboardId,
    "Keyboard reorder stays below pins",
  );
  const beforeFailure = await chatOrder();
  await page.evaluate(() => {
    window.originalSetItem = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (key.startsWith("codex-sidebar-order:"))
        throw new Error("Storage full fixture");
      return window.originalSetItem.call(this, key, value);
    };
  });
  await chatButton(keyboardId).press("Alt+ArrowDown");
  assert.deepEqual(
    await chatOrder(),
    beforeFailure,
    "Failed persistence does not report a saved reorder",
  );
  await page
    .getByText(/Could not save sidebar order:.*Storage full fixture/)
    .waitFor();
  await page.evaluate(() => {
    Storage.prototype.setItem = window.originalSetItem;
  });
  const savedChatOrder = await chatOrder();
  // Cross-project drags cannot change an agent's working directory.
  await chatButton(keyboardId).dragTo(
    group("litos").locator(".project-tree-toggle"),
  );
  assert.equal(
    (await state()).find((a) => a.id === keyboardId).cwd,
    folders.assistant,
  );
  await page.reload();
  await group("assistant").waitFor();
  assert.deepEqual(
    await projectOrder(),
    movedProjects,
    "Project order survives reload",
  );
  await group("assistant").getByText("Show more", { exact: true }).click();
  assert.deepEqual(
    await chatOrder(),
    savedChatOrder,
    "Chat order and pin survive reload",
  );
  await chatButton(pinnedId).hover();
  await page
    .getByRole("button", { name: `Unpin ${pinnedName}`, exact: true })
    .click();
  await waitFor(
    async () => !(await state()).find((a) => a.id === pinnedId).pinned,
  );
  await group("assistant").locator(".project-tree-toggle").click();
  assert.equal(await group("assistant").locator("[data-chat]").count(), 0);
  await page.reload();
  await group("assistant").waitFor();
  assert.equal(await group("assistant").locator("[data-chat]").count(), 0);
  await page.getByLabel("Search chats").fill("Review component 2");
  await page.locator(`[data-chat="${created[2].id}"]`).click();
  await group("litos").locator(".project-tree-heading").hover();
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
