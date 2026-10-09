import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

for (const [theme, viewport] of [
  ["light", { width: 1440, height: 900 }],
  ["dark", { width: 1440, height: 900 }],
  ["mobile", { width: 390, height: 844 }],
]) {
  test(`the combined classic sidebar routes actions to their owner: ${theme}`, async ({
    page,
    context,
  }, testInfo) => {
    test.setTimeout(90000);
    page.setDefaultTimeout(10000);
    await page.setViewportSize(viewport);
    await page.emulateMedia({
      colorScheme: theme === "dark" ? "dark" : "light",
    });
    const calls = { local: [], remote: [] };
    const handler =
      (owner) =>
      ({ url, body, json, snapshot }) => {
        if (url.pathname === "/api/organization") {
          calls[owner].push({ path: url.pathname, body });
          const row = snapshot.threads.find((chat) => chat.id === body.id);
          expect(row).toBeTruthy();
          for (const key of ["pinned", "archived"])
            if (key in body) row[key] = body[key];
          if ("project_folder" in body) {
            row.projectFolder = body.project_folder;
            row.projectFolderRevision = (row.projectFolderRevision || 0) + 1;
          }
          json({
            projectFolder: row.projectFolder || null,
            pinned: !!row.pinned,
            archived: !!row.archived,
          });
          return true;
        }
        if (url.pathname === "/api/rename") {
          calls[owner].push({ path: url.pathname, body });
          const row = snapshot.threads.find((chat) => chat.id === body.id);
          expect(row).toBeTruthy();
          row.name = body.name;
          json({ ok: true });
          return true;
        }
        if (url.pathname === "/api/projects") {
          calls[owner].push({ path: url.pathname, body });
          const project = snapshot.runtime.projects.find(
            (row) => row.path === body.path,
          );
          expect(project).toBeTruthy();
          expect(body.expected_revision).toBe(project.organizationRevision);
          if (body.action === "rename_folder")
            project.folders.find((row) => row.id === body.folder_id).name =
              body.name;
          project.organizationRevision += 1;
          json({ project, revision: project.organizationRevision });
          return true;
        }
        return false;
      };
    const local = await fixture("Local", false, { handle: handler("local") });
    const remote = await fixture("Remote", true, { handle: handler("remote") });
    for (const [owner, server] of [
      ["local", local],
      ["remote", remote],
    ]) {
      const project = server.snapshot.runtime.projects[0];
      Object.assign(project, {
        id: owner === "local" ? "logical-project" : "remote-project",
        path: owner === "local" ? "/same/project" : "/remote/project",
        name: owner === "local" ? "Home project" : "Remote project",
        homeServerId: owner,
        organizationRevision: owner === "local" ? 3 : 9,
        peerTeamsRevision: owner === "local" ? 4 : 8,
        folders: [{ id: "folder", name: `${owner} folder`, parentId: null }],
        ...(owner === "remote"
          ? {
              projectAliases: [
                {
                  serverId: "local",
                  projectId: "logical-project",
                  name: "Home project",
                },
              ],
            }
          : {}),
      });
      Object.assign(server.snapshot.threads[0], {
        cwd: project.path,
        pinned: true,
        projectFolderRevision: 2,
        updated: Date.now() / 1000,
      });
      Object.assign(server.snapshot.runtime, {
        projectOrganizationVersion: 1,
        peerTeamsVersion: 1,
        sidebarOrder: { revision: 1, groups: {} },
      });
    }
    local.aliases.remote = "REM";
    local.discoverPeer(remote);
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    try {
      await context.addInitScript(
        ({ origin, destination }) => {
          const nativeFetch = window.fetch.bind(window);
          window.fetch = async (input, init) => {
            const request = new Request(input, init);
            const url = new URL(request.url);
            if (url.origin !== origin) return nativeFetch(request);
            const body = await request.clone().arrayBuffer();
            return nativeFetch(destination + url.pathname + url.search, {
              method: request.method,
              headers: request.headers,
              ...(body.byteLength ? { body } : {}),
              signal: request.signal,
              redirect: "error",
              credentials: "omit",
            });
          };
        },
        { origin: remote.invitation.origin, destination: remote.origin },
      );
      await page.goto(local.origin + "/?studio-navigation=combined");
      if (viewport.width < 760)
        await page
          .getByRole("button", { name: "Servers and chats", exact: true })
          .click();
      const sidebar = page.locator("#sidebar");
      await expect(
        sidebar.getByRole("button", { name: /Remote chat/ }).first(),
      ).toBeVisible();
      await expect(
        sidebar.getByRole("button", { name: /Local chat/ }).first(),
      ).toBeVisible();
      await expect(sidebar.locator(".sidebar-project")).toHaveCount(1);
      await expect(
        sidebar.getByRole("button", { name: "Home project", exact: true }),
      ).toBeVisible();
      await expect(page.getByLabel("Studio server")).toHaveCount(0);
      await expect(
        sidebar.getByRole("button", { name: "Studio settings", exact: true }),
      ).toBeVisible();
      await expect(sidebar.locator("[data-folder-id]")).toHaveCount(2);
      expect(calls).toEqual({ local: [], remote: [] });
      await page.screenshot({
        path: testInfo.outputPath(`combined-${theme}.png`),
      });

      let row = sidebar
        .locator(".sidebar-row")
        .filter({ hasText: "Remote chat" });
      await row.hover();
      await row
        .getByRole("button", { name: "Actions for Remote chat", exact: true })
        .click();
      await page.getByRole("menuitem", { name: "Unpin", exact: true }).click();
      await expect(row.locator(".chat-pin")).toHaveCount(0);
      await expect.poll(() => calls.remote.length).toBe(1);
      expect(calls.remote[0]).toEqual({
        path: "/api/organization",
        body: { id: "overlap", pinned: false },
      });
      expect(local.snapshot.threads[0].pinned).toBe(true);

      await row.hover();
      await row
        .getByRole("button", { name: "Actions for Remote chat", exact: true })
        .click();
      await page.getByRole("menuitem", { name: "Rename", exact: true }).click();
      await row.getByLabel("Chat name").fill("Remote renamed");
      await row.getByRole("button", { name: "Save name" }).click();
      row = sidebar
        .locator(".sidebar-row")
        .filter({ hasText: "Remote renamed" });
      await expect(
        row.getByRole("button", { name: /Remote renamed/ }).first(),
      ).toBeVisible();
      expect(calls.remote[1].body.id).toBe("overlap");
      expect(calls.remote[1].body.request_id).toBeTruthy();
      expect(local.snapshot.threads[0].name).toBe("Local chat");

      await row.hover();
      await row
        .getByRole("button", {
          name: "Actions for Remote renamed",
          exact: true,
        })
        .click();
      await page
        .getByRole("menuitem", { name: "Move to folder", exact: true })
        .click();
      await page
        .getByLabel("Move to folder", { exact: true })
        .selectOption({ label: "remote folder" });
      await page
        .getByRole("button", { name: "Move chat", exact: true })
        .click();
      await expect(
        page.getByRole("dialog").filter({
          has: page.getByRole("heading", { name: "Move chat", exact: true }),
        }),
      ).toHaveCount(0);
      await expect(
        row.locator("xpath=ancestor::*[@data-folder-id][1]"),
      ).toHaveAttribute(
        "data-folder-id",
        JSON.stringify(["folder", "remote", "/remote/project", "folder"]),
      );
      expect(calls.remote[2].body).toEqual({
        id: "overlap",
        project_path: "/remote/project",
        project_folder: "folder",
        expected_folder: null,
        expected_revision: 2,
      });
      expect(local.snapshot.threads[0].projectFolder).toBeUndefined();
      expect(calls.local).toEqual([]);

      await sidebar
        .getByRole("button", { name: /remote folder Codex/ })
        .hover();
      await sidebar
        .getByRole("button", {
          name: "Options for folder remote folder",
          exact: true,
        })
        .click();
      await page
        .getByRole("menuitem", { name: "Rename folder", exact: true })
        .click();
      await expect(
        page
          .getByRole("dialog")
          .filter({
            has: page.getByRole("heading", {
              name: "Rename folder",
              exact: true,
            }),
          })
          .locator(".project-directory"),
      ).toHaveText("/remote/project");
      await page
        .getByRole("textbox", { name: "Folder name", exact: true })
        .fill("Remote folder renamed");
      await page
        .getByRole("button", { name: "Save name", exact: true })
        .click();
      await expect(
        page.getByRole("dialog").filter({
          has: page.getByRole("heading", {
            name: "Rename folder",
            exact: true,
          }),
        }),
      ).toHaveCount(0);
      expect(calls.remote[3].body).toMatchObject({
        action: "rename_folder",
        path: "/remote/project",
        folder_id: "folder",
        expected_revision: 9,
      });
      expect(local.snapshot.runtime.projects[0].folders[0].name).toBe(
        "local folder",
      );
      await expect(
        sidebar.getByRole("button", { name: /Remote folder renamed Codex/ }),
      ).toBeVisible();

      await row
        .getByRole("button", { name: /Remote renamed/ })
        .first()
        .click();
      await expect(
        page.locator('iframe[title="Studio on Remote"]'),
      ).toBeVisible();
      if (viewport.width < 760)
        await page
          .getByRole("button", { name: "Servers and chats", exact: true })
          .click();
      await sidebar
        .getByLabel("Filter projects and chats")
        .fill("/remote/project");
      await expect(row).toBeVisible();
      await expect(
        sidebar.locator(".sidebar-row").filter({ hasText: "Local chat" }),
      ).toHaveCount(0);
      await sidebar.getByLabel("Filter projects and chats").fill("");
      remote.offline(true);
      await expect(async () => {
        await row.hover();
        await row
          .getByRole("button", {
            name: "Actions for Remote renamed",
            exact: true,
          })
          .click();
        await expect(
          page.getByRole("menuitem", { name: "Pin", exact: true }),
        ).toHaveAttribute("data-disabled", "true");
        await page.keyboard.press("Escape");
      }).toPass({ timeout: 15000 });
      await expect(row).toBeVisible();
      expect(calls.local).toEqual([]);
      expect(errors).toEqual([]);
      await testInfo.attach("owner-requests", {
        body: JSON.stringify(calls, null, 2),
        contentType: "application/json",
      });
    } finally {
      await remote.close();
      await local.close();
    }
  });
}

test("offline folder expansion does not change the saved source state after reconnect", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(60000);
  await page.setViewportSize({ width: 1440, height: 900 });
  const preferenceWrites = [];
  const local = await fixture("Local");
  const remote = await fixture("Remote", true, {
    handle: ({ request, url, body }) => {
      if (request.method === "POST" && url.pathname === "/api/sync/preferences")
        preferenceWrites.push(body);
      return false;
    },
  });
  const path = "/remote/project";
  const key = ":server:remote:codex-project-tree:/same/state";
  const saved = { [JSON.stringify([path, "folder", "folder"])]: true };
  Object.assign(remote.snapshot.runtime.projects[0], {
    path,
    name: "Remote project",
    folders: [{ id: "folder", name: "Saved closed folder" }],
  });
  Object.assign(remote.snapshot.threads[0], {
    cwd: path,
    projectFolder: "folder",
    updated: Date.now() / 1000,
  });
  Object.assign(remote.snapshot.runtime, {
    projectOrganizationVersion: 1,
    sidebarOrder: { revision: 1, groups: {} },
  });
  local.discoverPeer(remote);
  try {
    await context.addInitScript(
      ({ origin, destination, key, saved }) => {
        if (
          new URLSearchParams(location.search).get("studio-server") === "remote"
        )
          localStorage.setItem(key, JSON.stringify(saved));
        const nativeFetch = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init);
          const url = new URL(request.url);
          if (url.origin !== origin) return nativeFetch(request);
          const body = await request.clone().arrayBuffer();
          return nativeFetch(destination + url.pathname + url.search, {
            method: request.method,
            headers: request.headers,
            ...(body.byteLength ? { body } : {}),
            signal: request.signal,
            redirect: "error",
            credentials: "omit",
          });
        };
      },
      {
        origin: remote.invitation.origin,
        destination: remote.origin,
        key,
        saved,
      },
    );
    await page.goto(local.origin + "/?studio-navigation=combined");
    const sidebar = page.locator("#sidebar");
    const folder = sidebar.getByRole("button", {
      name: "Saved closed folder",
      exact: true,
    });
    await expect(folder).toHaveAttribute("aria-expanded", "false");
    const remoteFrame = page.frameLocator('iframe[title="Studio on Remote"]');
    const original = await remoteFrame
      .locator("html")
      .evaluate((_, key) => localStorage.getItem(key), key);
    expect(JSON.parse(original)).toEqual(saved);
    const writesBefore = preferenceWrites.length;
    remote.offline(true);
    const newChat = sidebar.getByRole("button", {
      name: "New chat in folder Saved closed folder",
      exact: true,
    });
    await expect(newChat).toBeDisabled();
    await folder.click();
    await expect(folder).toHaveAttribute("aria-expanded", "true");
    await expect(
      sidebar.locator(".sidebar-row").filter({ hasText: "Remote chat" }),
    ).toBeVisible();
    await page.screenshot({
      path: testInfo.outputPath("offline-expanded.png"),
    });
    expect(
      await remoteFrame
        .locator("html")
        .evaluate((_, key) => localStorage.getItem(key), key),
    ).toBe(original);
    remote.offline(false);
    remote.complete("fresh-after-reconnect");
    await expect(newChat).toBeEnabled({ timeout: 20000 });
    await expect(folder).toHaveAttribute("aria-expanded", "false", {
      timeout: 20000,
    });
    expect(
      await remoteFrame
        .locator("html")
        .evaluate((_, key) => localStorage.getItem(key), key),
    ).toBe(original);
    expect(preferenceWrites.length).toBe(writesBefore);
    expect(remote.writes).toEqual([]);
    expect(local.writes).toEqual([]);
  } finally {
    await remote.close();
    await local.close();
  }
});
