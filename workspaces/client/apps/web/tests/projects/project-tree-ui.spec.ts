declare global {
  interface Window {
    projectTreeFixture: { originalSetItem: Storage["setItem"] };
  }
}
import {
  test,
  expect,
  spawnFixture as spawn,
  readTestState,
  rustUiFixture,
} from "../playwright.ts";
// Production client with an isolated runtime. No model service or user state.
import assert from "node:assert/strict";
import { mkdtemp, mkdir, realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { _electron } from "@playwright/test";
import type { ElectronApplication, Page } from "@playwright/test";
import type { PostBody, PostResult } from "../../src/api.ts";
import type { ApiPostPath } from "../../src/apiTypes.ts";
import { fixturePostBody } from "../misc-fixture.ts";
import { required } from "../../src/components/componentTestFixtures.ts";
import { waitForFixturePort } from "../fixture-readiness.ts";
test("project pointer and keyboard order persist, and a click collapses the project", async ({
  page: runnerPage,
}) => {
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
      model: "gpt-6-luna",
      displayName: "Luna",
      levels: ["low", "medium", "high"],
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
  const proc = spawn(process.execPath, [rustUiFixture, root], {
    stdio: ["pipe", "pipe", "pipe"],
    env: {
      ...process.env,
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
      PROJECT_TREE_UI_FIXTURE: "1",
    },
  });
  let browser: ElectronApplication | undefined;
  let log = "";
  proc.stderr?.on("data", (d) => (log += d));
  try {
    const port = await waitForFixturePort(proc, { diagnostics: () => log });
    const url = `http://127.0.0.1:${port}`;
    let page: Page;
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
          BrowserWindow.getAllWindows()[0]?.isVisible(),
        ),
        false,
      );
    } else {
      page = runnerPage;
      await page.setViewportSize({ width: 1440, height: 960 });
      const canonicalRoot = await realpath(root);
      await page.addInitScript(
        ({ scope, canonical }) => {
          localStorage.setItem(
            `codex-sidebar-order:${scope}`,
            JSON.stringify({
              projects: [
                `${canonical}/assistant`,
                `${canonical}/Newcrom Case`,
                `${canonical}/litos`,
              ],
            }),
          );
        },
        { scope: root, canonical: canonicalRoot },
      );
    }
    const errors: string[] = [];
    const sidebarMigrations: { token: string | undefined }[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    page.on("request", (request) => {
      if (
        request.method() !== "POST" ||
        new URL(request.url()).pathname !== "/api/projects"
      )
        return;
      try {
        const body = fixturePostBody<"/api/projects">(request.postDataJSON());
        if (body.action === "reorder" && body.migration)
          sidebarMigrations.push({
            token: request.headers()["x-canvas-token"],
          });
      } catch {
        // Other project requests need not contain JSON migration bodies.
      }
    });
    if (process.env.CODEX_TEST_DESKTOP) {
      await page.waitForURL(url + "/");
      await page.locator("#message").waitFor();
    }
    const state = async () => (await readTestState(url)).runtime.agents;
    const waitFor = async <T>(predicate: () => Promise<T>) => {
      for (let i = 0; i < 100; i++) {
        const result = await predicate();
        if (result) return result;
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      throw Error("Timed out: " + log);
    };
    const token = (await readTestState(url)).token;
    const post = async <Path extends ApiPostPath>(
      path: Path,
      body: PostBody<Path>,
    ): Promise<PostResult<Path>> => {
      const response = await fetch(url + path, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": token,
          Origin: url,
        },
        body: JSON.stringify(body),
      });
      // The response comes from the repository-owned runtime fixture.
      const data: PostResult<Path> = await response.json();
      assert.ok(response.ok, JSON.stringify(data));
      return data;
    };
    const folders: Record<string, string> = {};
    const setupStartedAt = performance.now();
    const folderNames = ["assistant", "litos", "Newcrom Case", "Empty project"];
    await Promise.all(
      folderNames.map(async (name) => {
        const path = join(root, name);
        await mkdir(path);
        folders[name] = await realpath(path);
      }),
    );
    await Promise.all(
      ["assistant", "litos", "Newcrom Case"].map((name) =>
        post("/api/projects", { path: required(folders[name]) }),
      ),
    );
    const [created, legal] = await Promise.all([
      Promise.all(
        Array.from({ length: 7 }, (_, i) =>
          post("/api/leads", {
            id: crypto.randomUUID(),
            cwd: required(folders.assistant),
          }).then((chat) => ({ ...chat, name: `Review component ${i}` })),
        ),
      ),
      post("/api/leads", {
        id: crypto.randomUUID(),
        cwd: required(folders["Newcrom Case"]),
      }),
    ]);
    await Promise.all([
      ...created.map((chat) =>
        post("/api/rename", { id: chat.id, name: chat.name }),
      ),
      post("/api/rename", { id: legal.id, name: "Review evidence" }),
    ]);
    console.log(
      `Project tree fixture setup completed in ${Math.round(performance.now() - setupStartedAt)} ms`,
    );
    const stateDir = (await readTestState(url)).stateDir;
    if (process.env.CODEX_TEST_DESKTOP) {
      await page.evaluate(
        ({ stateDir, path }) =>
          localStorage.setItem(
            `codex-project-tree:${stateDir}`,
            JSON.stringify({ [path]: true }),
          ),
        { stateDir, path: required(folders.assistant) },
      );
      await page.reload({ waitUntil: "domcontentloaded" });
    } else {
      await page.addInitScript(
        ({ stateDir, path }) =>
          localStorage.setItem(
            `codex-project-tree:${stateDir}`,
            JSON.stringify({ [path]: true }),
          ),
        { stateDir, path: required(folders.assistant) },
      );
      await page.goto(url, { waitUntil: "domcontentloaded" });
    }
    const group = (name: string) =>
      page.locator(".sidebar-project").filter({
        has: page.locator(".project-tree-toggle", { hasText: name }),
      });
    await group("assistant").waitFor();
    const projectsHeading = page.locator(".projects-heading-label");
    assert.equal(await projectsHeading.textContent(), "Projects");
    assert.equal(await projectsHeading.getAttribute("aria-expanded"), null);
    const projectName = group("assistant").locator(".project-tree-toggle");
    assert.equal(await projectName.evaluate((node) => node.tagName), "BUTTON");
    // The saved bare path of the earlier project collapse leaves it open.
    assert.equal(await projectName.getAttribute("aria-expanded"), "true");
    const initialChats = group("assistant").locator("[data-chat]");
    // The list fills in after reload, so wait for the full set before counting.
    await waitFor(async () => (await initialChats.count()) === 7);
    await projectName.click();
    await waitFor(async () => (await initialChats.count()) === 0);
    assert.equal(await projectName.getAttribute("aria-expanded"), "false");
    await projectName.click();
    await waitFor(async () => (await initialChats.count()) === 7);
    assert.equal(await initialChats.count(), 7);
    assert.equal(
      await group("litos").getByText("No chats", { exact: true }).count(),
      1,
    );
    const projectOrder = () =>
      page
        .locator(".sidebar-project")
        .evaluateAll((rows) =>
          rows.map((row) => row.getAttribute("data-project-path")),
        );
    const chatOrder = () =>
      group("assistant")
        .locator("[data-chat]")
        .evaluateAll((rows) =>
          rows.map((row) => row.getAttribute("data-chat")),
        );
    const waitForOrderReceipt = () =>
      expect
        .poll(() =>
          page.evaluate(() =>
            Object.keys(localStorage).some(
              (key) =>
                key.startsWith("codex-sidebar-order:") &&
                key.endsWith(":pending"),
            ),
          ),
        )
        .toBe(false);
    const beforeProjects = await projectOrder();
    await waitFor(
      async () =>
        (await readTestState(url)).runtime.sidebarOrder?.groups !== null,
    );
    await waitFor(async () => sidebarMigrations.length > 0);
    assert.ok(sidebarMigrations.length > 0, "Sidebar migration was sent");
    assert.ok(
      sidebarMigrations.every(({ token }) => token),
      "Sidebar migration waits for the session token",
    );
    if (!process.env.CODEX_TEST_DESKTOP) {
      const migrated = required(
        (await readTestState(url)).runtime.sidebarOrder,
      );
      assert.ok(migrated.groups);
      assert.deepEqual(migrated.groups.projects, [
        required(folders.assistant),
        required(folders["Newcrom Case"]),
        required(folders.litos),
      ]);
    }
    const secondContext = process.env.CODEX_TEST_DESKTOP
      ? null
      : await page
          .context()
          .browser()
          ?.newContext({ viewport: { width: 1440, height: 960 } });
    const secondPage = secondContext ? await secondContext.newPage() : null;
    if (secondPage) {
      await secondPage.goto(url, { waitUntil: "domcontentloaded" });
      await secondPage.locator(".sidebar-project").first().waitFor();
    }
    await group("litos")
      .locator(".project-tree-toggle")
      .dragTo(group("assistant").locator(".project-tree-toggle"), {
        targetPosition: { x: 45, y: 3 },
      });
    const movedProjects = await projectOrder();
    assert.ok(
      movedProjects.indexOf(required(folders.litos)) <
        movedProjects.indexOf(required(folders.assistant)),
    );
    assert.notDeepEqual(movedProjects, beforeProjects);
    await waitForOrderReceipt();
    const assistantProject = group("assistant").locator(".project-tree-toggle");
    await assistantProject.focus();
    await page.keyboard.press("Alt+ArrowUp");
    const keyboardProjects = await projectOrder();
    assert.ok(
      keyboardProjects.indexOf(required(folders.assistant)) <
        keyboardProjects.indexOf(required(folders.litos)),
    );
    await waitForOrderReceipt();
    if (secondPage) {
      await expect
        .poll(() =>
          secondPage
            .locator(".sidebar-project")
            .evaluateAll((rows) =>
              rows.map((row) => row.getAttribute("data-project-path")),
            ),
        )
        .toEqual(keyboardProjects);
      await secondPage.reload();
      await expect
        .poll(() =>
          secondPage
            .locator(".sidebar-project")
            .evaluateAll((rows) =>
              rows.map((row) => row.getAttribute("data-project-path")),
            ),
        )
        .toEqual(keyboardProjects);
    }
    const beforeChats = await chatOrder();
    const chatButton = (id: string | null | undefined) =>
      page.locator(`[data-chat="${id}"]`);
    await chatButton(beforeChats.at(-1)).dragTo(chatButton(beforeChats[0]), {
      targetPosition: { x: 45, y: 3 },
    });
    assert.equal(
      (await chatOrder())[0],
      beforeChats.at(-1),
      "Chat drag persists visual order",
    );
    if (secondPage) {
      await expect
        .poll(() =>
          secondPage
            .locator(
              `[data-project-path="${required(folders.assistant)}"] [data-chat]`,
            )
            .evaluateAll((rows) =>
              rows.map((row) => row.getAttribute("data-chat")),
            ),
        )
        .toEqual(await chatOrder());
    }
    const pinnedId = beforeChats[3];
    const pinnedName = required(
      (await state()).find((a) => a.id === pinnedId),
    ).name;
    await chatButton(pinnedId).hover();
    await page
      .getByRole("button", { name: `Pin ${pinnedName}`, exact: true })
      .click();
    await waitFor(
      async () =>
        required((await state()).find((a) => a.id === pinnedId)).pinned,
    );
    await page.waitForFunction(
      (id) =>
        document
          .querySelector(
            `.sidebar-project[data-project-path="${CSS.escape(id.path)}"] [data-chat]`,
          )
          ?.getAttribute("data-chat") === id.chat,
      { path: required(folders.assistant), chat: pinnedId },
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
      required(resting).width >= required(hovering).width + 40,
      "Hidden controls return title space",
    );
    await page.mouse.move(1100, 100);
    const nonMenuActions = group("assistant").locator(
      '.project-tree-action:not([aria-haspopup="menu"])',
    );
    assert.ok(
      (await nonMenuActions.count()) > 0,
      "Project creation action exists",
    );
    assert.equal(
      await nonMenuActions.evaluateAll((elements) =>
        elements.every((el) => getComputedStyle(el).opacity === "0"),
      ),
      true,
      "Expanded projects hide non-menu actions without hover",
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
    // The visual order is optimistic; wait for its receipt before the next move.
    await waitForOrderReceipt();
    const beforeFailure = await chatOrder();
    await page.evaluate(() => {
      window.projectTreeFixture = {
        originalSetItem: Storage.prototype.setItem,
      };
      Storage.prototype.setItem = function (key, value) {
        if (key.startsWith("codex-sidebar-order:"))
          throw new Error("Storage full fixture");
        return window.projectTreeFixture.originalSetItem.call(this, key, value);
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
      Storage.prototype.setItem = window.projectTreeFixture.originalSetItem;
    });
    const savedChatOrder = await chatOrder();
    // Cross-project drags cannot change an agent's working directory.
    await chatButton(keyboardId).dragTo(
      group("litos").locator(".project-tree-toggle"),
    );
    assert.equal(
      required((await state()).find((a) => a.id === keyboardId)).cwd,
      required(folders.assistant),
    );
    await page.reload({ waitUntil: "domcontentloaded" });
    await group("assistant").waitFor();
    assert.deepEqual(
      await projectOrder(),
      keyboardProjects,
      "Project order after pointer and keyboard moves survives reload",
    );

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
      async () =>
        !required((await state()).find((a) => a.id === pinnedId)).pinned,
    );
    assert.equal(await assistantProject.getAttribute("aria-expanded"), "true");
    const assistantChatCount = await group("assistant")
      .locator("[data-chat]")
      .count();
    await assistantProject.click();
    await waitFor(
      async () =>
        (await group("assistant").locator("[data-chat]").count()) === 0,
    );
    await assistantProject.click();
    await waitFor(
      async () =>
        (await group("assistant").locator("[data-chat]").count()) ===
        assistantChatCount,
    );
    assert.equal(
      await group("assistant").locator("[data-chat]").count(),
      assistantChatCount,
      "A second click on a project name shows its chats again",
    );
    await page.locator(`[data-chat="${legal.id}"]`).click();
    await page.reload({ waitUntil: "domcontentloaded" });
    await group("assistant").waitFor();
    assert.equal(
      await group("assistant")
        .locator(".project-tree-toggle")
        .getAttribute("aria-expanded"),
      "true",
    );
    assert.equal(
      await group("assistant").locator("[data-chat]").count(),
      assistantChatCount,
    );
    await page
      .getByLabel("Filter projects and chats")
      .fill("Review component 2");
    await page.locator(`[data-chat="${required(created[2]).id}"]`).click();
    // The previous reload also cleared this filter implicitly. Clear it
    // explicitly so the next interaction targets the now-visible project.
    await page.getByLabel("Filter projects and chats").fill("");
    await group("litos").locator(".project-tree-heading").hover();
    await page
      .getByRole("button", { name: "New chat in litos", exact: true })
      .click();
    const chatDialog = page.getByRole("dialog", {
      name: "New chat",
      exact: true,
    });
    await expect(
      chatDialog.getByRole("button", { name: /Studio server.*Active.*litos/ }),
    ).toHaveAttribute("aria-pressed", "true");
    await chatDialog
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    await waitFor(
      async () => (await state()).filter((a) => a.isLead).length === 11,
    );
    assert.equal(
      required((await state()).find((a) => a.id === required(created[2]).id))
        .cwd,
      required(folders.assistant),
      "New chat does not move the previous chat",
    );
    await page
      .getByRole("button", { name: "Add project", exact: true })
      .click();
    const dialog = page.getByRole("dialog", {
      name: "Add project",
      exact: true,
    });
    await dialog
      .getByLabel("Folder path")
      .fill(required(folders["Empty project"]));
    const directoryReadStartedAt = performance.now();
    const directoryRead = page.waitForResponse((response) => {
      const request = response.request();
      const url = new URL(response.url());
      return (
        request.method() === "GET" &&
        url.pathname === "/api/directories" &&
        url.searchParams.get("path") === required(folders["Empty project"])
      );
    });
    await dialog
      .getByRole("button", { name: "Open path", exact: true })
      .click();
    const directoryResponse = await directoryRead;
    assert.equal(directoryResponse.status(), 200);
    const directoryReadMs = Math.round(
      performance.now() - directoryReadStartedAt,
    );
    console.log(`Project directory GET completed in ${directoryReadMs} ms`);
    await dialog
      .getByRole("button", { name: "Add project", exact: true })
      .click();
    await dialog.waitFor({ state: "hidden" });
    await group("Empty project")
      .getByText("No chats", { exact: true })
      .waitFor();
    const projectOptions = async (name: string) => {
      await group(name).locator(":scope > .project-tree-heading").hover();
      await page
        .getByRole("button", {
          name: `Options for project ${name}`,
          exact: true,
        })
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
      await realpath(required(folders.litos)),
      required(folders.litos),
      "project rename preserves the directory",
    );
    await secondContext?.close();
    expect(errors).toEqual([]);
    console.log(
      "PASS project tree: project names, add and rename projects, chat order and pinning, keyboard controls, new chats in projects. Evidence " +
        root,
    );
  } finally {
    await browser?.close();
  }
});
