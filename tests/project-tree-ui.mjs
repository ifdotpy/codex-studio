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
  await waitFor(
    async () => (await group("assistant").locator("[data-chat]").count()) === 5,
  );
  assert.equal(await group("assistant").locator("[data-chat]").count(), 5);
  await group("assistant").getByText("Show more", { exact: true }).click();
  await waitFor(
    async () => (await group("assistant").locator("[data-chat]").count()) === 7,
  );
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
      .evaluateAll((elements) =>
        elements.every((el) => getComputedStyle(el).opacity === "0"),
      ),
    true,
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
  await page.getByLabel("Filter projects and chats").fill("Review component 2");
  await page.locator(`[data-chat="${created[2].id}"]`).click();
  await group("litos").locator(".project-tree-heading").hover();
  await page
    .getByRole("button", { name: "New chat in litos", exact: true })
    .click();
  await waitFor(
    async () => (await state()).filter((a) => a.isLead).length === 11,
  );
  assert.equal(
    (await state()).find((a) => a.id === created[2].id).cwd,
    folders.assistant,
    "New chat does not move the previous chat",
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
  const projectOptions = async (name) => {
    await group(name).locator(":scope > .project-tree-heading").hover();
    await page
      .getByRole("button", { name: `Options for project ${name}`, exact: true })
      .click();
  };
  await projectOptions("litos");
  await page
    .getByRole("menuitem", { name: "Rename project", exact: true })
    .click();
  const renameProject = page.getByRole("dialog", {
    name: "Rename project",
    exact: true,
  });
  await renameProject.getByLabel("Project name").fill("Client work");
  await renameProject
    .getByRole("button", { name: "Save name", exact: true })
    .click();
  await renameProject.waitFor({ state: "hidden" });
  await group("Client work").waitFor();
  assert.equal(
    await realpath(folders.litos),
    folders.litos,
    "project rename preserves the directory",
  );
  const newFolder = async (parent, name) => {
    if (parent) {
      await page
        .locator(`[data-folder-id="${parent}"] > .project-tree-heading`)
        .hover();
      await page
        .locator(`[data-folder-id="${parent}"] > .project-tree-heading`)
        .getByRole("button", { name: /Options for folder/ })
        .click();
      await page
        .getByRole("menuitem", { name: "New subfolder", exact: true })
        .click();
    } else {
      await projectOptions("Client work");
      await page
        .getByRole("menuitem", { name: "New folder", exact: true })
        .click();
    }
    const form = page.getByRole("dialog", { name: "New folder", exact: true });
    await form.getByLabel("Folder name").fill(name);
    await form
      .getByRole("button", { name: "Create folder", exact: true })
      .click();
    await form.waitFor({ state: "hidden" });
    const projects = await fetch(url + "/api/projects").then((response) =>
      response.json(),
    );
    return projects.items
      .find((project) => project.path === folders.litos)
      .folders.find((folder) => folder.name === name).id;
  };
  const workFolder = await newFolder(null, "Reviews");
  const childFolder = await newFolder(workFolder, "Interface");
  const sourceChat = (await state()).find(
    (a) => a.isLead && a.cwd === folders.litos,
  );
  const openChatMenu = async () => {
    const row = page.locator(`[data-chat="${sourceChat.id}"]`).locator("..");
    await row.hover();
    await row
      .getByRole("button", {
        name: `Actions for ${sourceChat.name}`,
        exact: true,
      })
      .click();
  };
  await openChatMenu();
  await page
    .getByRole("menuitem", { name: "Move to folder", exact: true })
    .click();
  const moveDialog = page.getByRole("dialog", {
    name: "Move chat",
    exact: true,
  });
  await moveDialog.getByLabel("Move to folder").selectOption(childFolder);
  await moveDialog
    .getByRole("button", { name: "Move chat", exact: true })
    .click();
  await moveDialog.waitFor({ state: "hidden" });
  await page
    .locator(`[data-folder-id="${childFolder}"] [data-chat="${sourceChat.id}"]`)
    .waitFor();
  const movedChat = (await state()).find((a) => a.id === sourceChat.id);
  assert.equal(movedChat.cwd, sourceChat.cwd);
  assert.equal(movedChat.accountKey, sourceChat.accountKey);
  const folderHeading = (id) =>
    page.locator(`[data-folder-id="${id}"] > .project-tree-heading`);
  const moveSource = () => page.locator(`[data-chat="${sourceChat.id}"]`);
  const currentSource = async () =>
    (await state()).find((a) => a.id === sourceChat.id);
  // A rejected move keeps the chat at its saved location.
  await page.route("**/api/organization", (route) =>
    route.fulfill({
      status: 409,
      contentType: "application/json",
      body: JSON.stringify({ error: "Folder changed in another window" }),
    }),
  );
  const dragChatTo = async (target) => {
    await moveSource().hover();
    const box = await moveSource().boundingBox();
    await page.mouse.down();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2 + 10, {
      steps: 5,
    });
    await target.hover();
    await target.hover();
    assert.equal(
      await target.getAttribute("data-folder-drop"),
      "true",
      "The destination has a visible drop highlight",
    );
    await page.screenshot({
      path: join(root, "folder-drop.png"),
      animations: "disabled",
    });
    await page.mouse.up();
    assert.equal(await page.locator('[data-folder-drop="true"]').count(), 0);
  };
  await dragChatTo(folderHeading(workFolder));
  await page
    .getByText("Folder changed in another window", { exact: true })
    .waitFor();
  assert.equal((await currentSource()).projectFolder, childFolder);
  await page.unroute("**/api/organization");
  await dragChatTo(folderHeading(workFolder));
  await waitFor(
    async () => (await currentSource()).projectFolder === workFolder,
  );
  await folderHeading(workFolder)
    .locator("..")
    .locator(
      `:scope > .project-folder-chats > .sidebar-row [data-chat="${sourceChat.id}"]`,
    )
    .waitFor();
  await dragChatTo(
    group("Client work").locator(":scope > .project-tree-heading"),
  );
  await waitFor(async () => !(await currentSource()).projectFolder);
  await group("Client work")
    .locator(
      `:scope > .project-chats > .sidebar-row [data-chat="${sourceChat.id}"]`,
    )
    .waitFor();
  // Dropping onto a collapsed destination opens it after the server confirms.
  await folderHeading(childFolder)
    .getByRole("button", { expanded: true })
    .click();
  await dragChatTo(folderHeading(childFolder));
  await waitFor(
    async () => (await currentSource()).projectFolder === childFolder,
  );
  await page
    .locator(`[data-folder-id="${childFolder}"] [data-chat="${sourceChat.id}"]`)
    .waitFor();
  assert.equal((await currentSource()).cwd, sourceChat.cwd);
  assert.equal((await currentSource()).accountKey, sourceChat.accountKey);
  assert.equal((await currentSource()).threadId, sourceChat.threadId);
  await page
    .locator(`[data-folder-id="${childFolder}"] > .project-tree-heading`)
    .hover();
  await page
    .getByRole("button", { name: "New chat in folder Interface", exact: true })
    .click();
  await waitFor(async () =>
    (await state()).some(
      (a) =>
        a.isLead && a.id !== sourceChat.id && a.projectFolder === childFolder,
    ),
  );
  await page.reload();
  await group("Client work").waitFor();
  await page
    .locator(`[data-folder-id="${childFolder}"] [data-chat="${sourceChat.id}"]`)
    .waitFor();
  await waitFor(
    async () =>
      (await page
        .locator(`[data-folder-id="${childFolder}"] [data-chat]`)
        .count()) === 2,
  );
  await page
    .locator(`[data-folder-id="${childFolder}"] > .project-tree-heading`)
    .hover();
  await page
    .getByRole("button", { name: "Options for folder Interface", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Rename folder", exact: true })
    .click();
  const renameFolder = page.getByRole("dialog", {
    name: "Rename folder",
    exact: true,
  });
  await renameFolder.getByLabel("Folder name").fill("UI review");
  await renameFolder
    .getByRole("button", { name: "Save name", exact: true })
    .click();
  await renameFolder.waitFor({ state: "hidden" });
  await page
    .locator(`[data-folder-id="${childFolder}"] > .project-tree-heading`)
    .getByText("UI review", { exact: true })
    .waitFor();
  const emptyFolder = await newFolder(null, "Temporary");
  await page
    .locator(`[data-folder-id="${emptyFolder}"] > .project-tree-heading`)
    .hover();
  await page
    .getByRole("button", { name: "Options for folder Temporary", exact: true })
    .click();
  await page
    .getByRole("menuitem", { name: "Remove empty folder", exact: true })
    .click();
  await page
    .locator(`[data-folder-id="${emptyFolder}"]`)
    .waitFor({ state: "detached" });
  await page.getByLabel("Filter projects and chats").fill("UI review");
  await page
    .locator(`[data-folder-id="${childFolder}"] [data-chat="${sourceChat.id}"]`)
    .waitFor();
  await page.getByLabel("Filter projects and chats").fill("");
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
    "PASS project tree: project and folder names, nested chat folders, chat moves, new chats in folders, empty folder removal, directory preservation, order, search, persistence, mobile widths. Evidence " +
      root,
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
