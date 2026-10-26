#!/usr/bin/env node
import {
  test,
  expect,
  spawnFixture as spawn,
  readTestState,
} from "../playwright.mjs";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

test("project folders create, move, rename and persist", async ({
  page: runnerPage,
}) => {
  const skill = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const { _electron } = createRequire(
    join(skill, "workspaces/client/apps/web/package.json"),
  )("playwright");
  const root = await mkdtemp(join(tmpdir(), "codex-project-folders-ui-"));
  const proc = spawn(
    "python3",
    [
      "-B",
      join(skill, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, PROJECT_TREE_UI_FIXTURE: "1" },
    },
  );
  let browser;
  let log = "";
  proc.stderr.on("data", (data) => (log += data));
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
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
      await page.waitForURL(url + "/");
      await page.locator("#message").waitFor();
    } else {
      page = runnerPage;
      await page.setViewportSize({ width: 1440, height: 960 });
    }

    const token = (await readTestState(url)).token;
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
    const projectPath = join(root, "Client work");
    await mkdir(projectPath);
    const canonicalProjectPath = await realpath(projectPath);
    await post("/api/projects", { path: canonicalProjectPath });
    const chat = await post("/api/leads", {
      id: crypto.randomUUID(),
      cwd: canonicalProjectPath,
    });
    const chatName = "Folder move source";
    await post("/api/rename", { id: chat.id, name: chatName });
    const stateDir = (await readTestState(url)).stateDir;
    const expandedTree = { [canonicalProjectPath]: true };
    if (process.env.CODEX_TEST_DESKTOP) {
      await page.evaluate(
        ({ stateDir, expandedTree }) =>
          localStorage.setItem(
            `codex-project-tree:${stateDir}`,
            JSON.stringify(expandedTree),
          ),
        { stateDir, expandedTree },
      );
      await page.reload({ waitUntil: "domcontentloaded" });
    } else {
      await page.addInitScript(
        ({ stateDir, expandedTree }) =>
          localStorage.setItem(
            `codex-project-tree:${stateDir}`,
            JSON.stringify(expandedTree),
          ),
        { stateDir, expandedTree },
      );
      await page.goto(url, { waitUntil: "domcontentloaded" });
    }

    const project = page.locator(".sidebar-project").filter({
      has: page.locator(".project-tree-toggle", { hasText: "Client work" }),
    });
    await project.locator(`[data-chat="${chat.id}"]`).waitFor();
    const state = async () => (await readTestState(url)).runtime.agents;
    const projectData = async () =>
      fetch(url + "/api/projects").then((response) => response.json());
    const createFolder = async (parentId, name) => {
      if (parentId) {
        const heading = page.locator(
          `[data-folder-id="${parentId}"] > .project-tree-heading`,
        );
        await heading.hover();
        await heading
          .getByRole("button", { name: /Options for folder/ })
          .click();
        await page
          .getByRole("menuitem", { name: "New subfolder", exact: true })
          .click();
      } else {
        await project.locator(":scope > .project-tree-heading").hover();
        await page
          .getByRole("button", { name: "Options for project Client work" })
          .click();
        await page
          .getByRole("menuitem", { name: "New chat folder", exact: true })
          .click();
      }
      await page.getByRole("menu").waitFor({ state: "hidden" });
      const dialog = page.getByRole("dialog", {
        name: "New chat folder",
        exact: true,
      });
      const overlay = dialog
        .locator("xpath=ancestor::*[contains(@class, 'mantine-Modal-root')]")
        .locator(".mantine-Modal-overlay");
      await dialog.getByLabel("Folder name").fill(name);
      await dialog
        .getByRole("button", { name: "Create folder", exact: true })
        .click();
      await dialog.waitFor({ state: "hidden" });
      await overlay.waitFor({ state: "detached" });
      const projects = await projectData();
      return projects.items
        .find((item) => item.path === canonicalProjectPath)
        .folders.find((folder) => folder.name === name).id;
    };

    const parentFolder = await createFolder(null, "Reviews");
    const childFolder = await createFolder(parentFolder, "Interface");
    const chatRow = page.locator(`[data-chat="${chat.id}"]`).locator("..");
    await chatRow.hover();
    await chatRow
      .getByRole("button", { name: `Actions for ${chatName}`, exact: true })
      .click();
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
      .locator(`[data-folder-id="${childFolder}"] [data-chat="${chat.id}"]`)
      .waitFor();
    const movedChat = (await state()).find((agent) => agent.id === chat.id);
    assert.equal(movedChat.cwd, canonicalProjectPath);
    assert.equal(movedChat.accountKey, chat.accountKey);

    const childHeading = page.locator(
      `[data-folder-id="${childFolder}"] > .project-tree-heading`,
    );
    await childHeading.hover();
    await page
      .getByRole("button", { name: "Options for folder Interface" })
      .click();
    await page
      .getByRole("menuitem", { name: "Rename folder", exact: true })
      .click();
    const renameDialog = page.getByRole("dialog", {
      name: "Rename folder",
      exact: true,
    });
    await renameDialog.getByLabel("Folder name").fill("UI review");
    await renameDialog
      .getByRole("button", { name: "Save name", exact: true })
      .click();
    await renameDialog.waitFor({ state: "hidden" });
    await childHeading.getByText("UI review", { exact: true }).waitFor();

    await page.reload({ waitUntil: "domcontentloaded" });
    await project.locator(`[data-chat="${chat.id}"]`).waitFor();
    await page
      .locator(`[data-folder-id="${childFolder}"] [data-chat="${chat.id}"]`)
      .waitFor();
    await childHeading.getByText("UI review", { exact: true }).waitFor();
    assert.equal(
      (await state()).find((agent) => agent.id === chat.id).projectFolder,
      childFolder,
    );

    const emptyFolder = await createFolder(null, "Temporary");
    const emptyHeading = page.locator(
      `[data-folder-id="${emptyFolder}"] > .project-tree-heading`,
    );
    await emptyHeading.hover();
    await page
      .getByRole("button", { name: "Options for folder Temporary" })
      .click();
    await page
      .getByRole("menuitem", { name: "Remove empty folder", exact: true })
      .click();
    await page.locator(`[data-folder-id="${emptyFolder}"]`).waitFor({
      state: "detached",
    });
    expect(await page.locator("[data-folder-id]").count()).toBe(2);
    console.log(
      "PASS project folders: create, move, rename, persistence, remove empty",
    );
  } finally {
    await browser?.close();
  }
});
