import { test, expect } from "../playwright.mjs";
import { sidebarParityFixture, sourcePath } from "./sidebar-parity-fixture.mjs";

const chat = (sidebar, name) =>
  sidebar.locator(".sidebar-row").filter({ hasText: name });
const button = (sidebar, name) =>
  sidebar.getByRole("button", { name, exact: true });
async function menu(page, sidebar, name, item) {
  const row = chat(sidebar, name);
  await row.hover();
  await button(row, `Actions for ${name}`).click();
  await page.getByRole("menuitem", { name: item, exact: true }).click();
}
async function projectMenu(page, sidebar, item) {
  await button(sidebar, "Home project").hover();
  await button(sidebar, "Options for project Home project").click();
  await page.getByRole("menuitem", { name: item, exact: true }).click();
}
async function listMenu(page, sidebar, item) {
  await button(sidebar, "Project list options").click();
  await page.getByRole("menuitem", { name: item, exact: true }).click();
}
async function teamMenu(page, sidebar, name, item) {
  const toggle = sidebar
    .getByRole("button", { name: new RegExp(name) })
    .first();
  await toggle.hover();
  await button(sidebar, `Options for team ${name}`).click();
  await page.getByRole("menuitem", { name: item, exact: true }).click();
}
async function folderMenu(page, sidebar, name, item) {
  await sidebar
    .getByRole("button", { name: new RegExp(name) })
    .first()
    .hover();
  await button(sidebar, `Options for folder ${name}`).click();
  await page.getByRole("menuitem", { name: item, exact: true }).click();
}
async function drag(page, from, to, edge = 0.5) {
  let a;
  await expect(async () => {
    await from.scrollIntoViewIfNeeded();
    a = await from.boundingBox();
    expect(a).not.toBeNull();
  }).toPass({ timeout: 5000 });
  await page.mouse.move(a.x + 18, a.y + a.height / 2);
  await page.mouse.down();
  await page.mouse.move(a.x + 22, a.y + a.height / 2 + 9, { steps: 6 });
  await to.scrollIntoViewIfNeeded();
  const b = await to.boundingBox();
  expect(b).not.toBeNull();
  await page.mouse.move(b.x + 18, b.y + b.height * edge, { steps: 20 });
  await page.mouse.move(b.x + 20, b.y + b.height * edge, { steps: 3 });
  await page.mouse.up();
}
async function setup(context, page) {
  test.setTimeout(90000);
  await page.setViewportSize({ width: 1440, height: 1100 });
  const fixture = await sidebarParityFixture(context);
  const sidebar = await fixture.open(page);
  await expect(chat(sidebar, "Remote Saved pin")).toBeVisible();
  await expect(button(sidebar, "Unpin Local Saved pin")).toBeEnabled();
  await expect(button(sidebar, "Unpin Remote Saved pin")).toBeEnabled();
  return { fixture, sidebar };
}

test("saved folders, nested folders, order, pins, teams, compact and collapse remain visible with search and status", async ({
  page,
  context,
}) => {
  const { fixture, sidebar } = await setup(context, page);
  try {
    await expect(sidebar.locator(".sidebar-project")).toHaveCount(1);
    await expect(sidebar.locator(".chat-pin")).toHaveCount(2);
    for (const owner of ["Local", "Remote"]) {
      const folder = sidebar
        .getByRole("button", { name: new RegExp(`${owner} Nested`) })
        .first();
      await expect(folder).toHaveAttribute("aria-expanded", "false");
      const team = sidebar
        .getByRole("button", { name: new RegExp(`${owner} Saved team`) })
        .first();
      await expect(team).toHaveAttribute("aria-expanded", "false");
      await expect(chat(sidebar, `${owner} Nested chat`)).toHaveCount(0);
      await expect(chat(sidebar, `${owner} Archived`)).toHaveCount(0);
      await expect(
        chat(sidebar, `${owner} Running`).locator('[role="img"]'),
      ).toHaveAttribute("aria-label", /Working/);
      await expect(
        chat(sidebar, `${owner} Approval`).locator('[role="img"]'),
      ).toHaveAttribute("aria-label", /Needs your answer/);
      await expect(
        chat(sidebar, `${owner} Unread`).locator('[role="img"]'),
      ).toHaveAttribute("aria-label", /Unread result/);
    }
    await expect(sidebar.getByLabel("2 unread chats")).toBeVisible();
    const sourceOrder = await sidebar
      .locator("[data-sidebar-item]")
      .evaluateAll((rows) =>
        rows.map((row) => row.getAttribute("data-sidebar-item")),
      );
    expect(
      sourceOrder.indexOf(JSON.stringify(["chat", "local", "root-b"])),
    ).toBeLessThan(
      sourceOrder.indexOf(JSON.stringify(["chat", "local", "root-a"])),
    );
    expect(
      sourceOrder.indexOf(JSON.stringify(["chat", "remote", "root-b"])),
    ).toBeLessThan(
      sourceOrder.indexOf(JSON.stringify(["chat", "remote", "root-a"])),
    );
    for (const [query, name] of [
      ["Remote Nested", "Remote Nested chat"],
      ["Remote Saved team", "Remote Team A"],
      ["Remote tail needle", "Remote Old 0"],
      ["/remote/project", "Remote Root A"],
    ]) {
      await sidebar.getByLabel("Filter projects and chats").fill(query);
      await expect(chat(sidebar, name)).toBeVisible();
      await expect(chat(sidebar, "Local Root A")).toHaveCount(0);
    }
    await sidebar.getByLabel("Filter projects and chats").fill("");
    await listMenu(page, sidebar, "Expand all projects");
    await expect(chat(sidebar, "Remote Nested chat")).toBeVisible();
    await expect(chat(sidebar, "Remote Team A")).toBeVisible();
    await sidebar.locator(".project-show-all").click();
    await expect(chat(sidebar, "Remote Old 5")).toBeVisible();
    await expect(sidebar.locator(".project-show-all")).toHaveText("Show less");
    await sidebar.locator(".project-show-all").click();
    await expect(chat(sidebar, "Remote Old 5")).toHaveCount(0);
    await listMenu(page, sidebar, "Show archived chats");
    await sidebar.getByLabel("Filter projects and chats").fill("Archived");
    await expect(chat(sidebar, "Local Archived")).toBeVisible();
    await expect(chat(sidebar, "Remote Archived")).toBeVisible();
    await sidebar.getByLabel("Filter projects and chats").fill("");
    await listMenu(page, sidebar, "Show active chats");
    expect(fixture.calls).toEqual({ local: [], remote: [] });
  } finally {
    await fixture.close();
  }
});

test("pointer and keyboard reorder preserve source order, and chats can enter and leave their own team", async ({
  page,
  context,
}) => {
  const { fixture, sidebar } = await setup(context, page);
  try {
    await listMenu(page, sidebar, "Expand all projects");
    const member = chat(sidebar, "Remote Team A").locator("[data-chat]");
    const other = chat(sidebar, "Remote Team B").locator("[data-chat]");
    await drag(page, other, member, 0.1);
    await expect
      .poll(
        () =>
          fixture.calls.remote.filter((call) => call.body.action === "reorder")
            .length,
      )
      .toBe(1);
    expect(fixture.calls.remote[0].body.groups.unknown).toEqual([
      "unknown-saved-id",
    ]);
    const before = await member.getAttribute("data-sidebar-group");
    await member.focus();
    await page.keyboard.press("Alt+ArrowUp");
    await expect
      .poll(
        () =>
          fixture.calls.remote.filter((call) => call.body.action === "reorder")
            .length,
      )
      .toBe(2);
    expect(await member.getAttribute("data-sidebar-group")).toBe(before);
    const remoteTeam = sidebar
      .getByRole("button", { name: /Remote Saved team/ })
      .first();
    await drag(
      page,
      chat(sidebar, "Remote Root A").locator("[data-chat]"),
      remoteTeam,
    );
    await expect
      .poll(() =>
        fixture.remote.snapshot.runtime.peerTeams[0].members.includes("root-a"),
      )
      .toBe(true);
    await expect(
      chat(sidebar, "Remote Root A").locator(
        "xpath=ancestor::*[@data-peer-team][1]",
      ),
    ).toHaveCount(1);
    await drag(
      page,
      chat(sidebar, "Remote Root A").locator("[data-chat]"),
      button(sidebar, "Home project"),
    );
    await expect
      .poll(() =>
        fixture.remote.snapshot.runtime.peerTeams[0].members.includes("root-a"),
      )
      .toBe(false);
    expect(
      fixture.calls.remote
        .filter((call) => call.body.action === "move")
        .map((call) => call.body),
    ).toEqual([
      expect.objectContaining({
        path: sourcePath("remote"),
        member: "root-a",
        team_id: "team",
        expected_revision: 8,
      }),
      expect.objectContaining({
        path: sourcePath("remote"),
        member: "root-a",
        team_id: null,
        expected_revision: 9,
      }),
    ]);
    const root = chat(sidebar, "Remote Root B").locator("[data-chat]");
    await drag(
      page,
      root,
      sidebar.getByRole("button", { name: /^Remote Empty/ }).first(),
    );
    await expect
      .poll(
        () =>
          fixture.remote.snapshot.threads.find((row) => row.id === "root-b")
            .projectFolder,
      )
      .toBe("empty");
    await drag(page, root, button(sidebar, "Home project"));
    await expect
      .poll(
        () =>
          fixture.remote.snapshot.threads.find((row) => row.id === "root-b")
            .projectFolder,
      )
      .toBe(null);
    expect(
      fixture.calls.remote
        .filter((call) => call.body.project_folder !== undefined)
        .map((call) => call.body),
    ).toEqual([
      expect.objectContaining({
        id: "root-b",
        project_path: sourcePath("remote"),
        project_folder: "empty",
        expected_revision: 2,
      }),
      expect.objectContaining({
        id: "root-b",
        project_path: sourcePath("remote"),
        project_folder: null,
        expected_revision: 3,
      }),
    ]);
    const localParent = sidebar.getByRole("button", {
      name: "Local Parent",
      exact: true,
    });
    const localEmpty = sidebar.getByRole("button", {
      name: "Local Empty",
      exact: true,
    });
    await drag(page, localEmpty, localParent, 0.1);
    await expect
      .poll(
        () =>
          fixture.calls.local.filter((call) => call.body.action === "reorder")
            .length,
      )
      .toBe(1);
    expect(fixture.local.snapshot.runtime.sidebarOrder.groups.unknown).toEqual([
      "unknown-saved-id",
    ]);
    await page.reload();
    await expect(
      chat(page.locator("#sidebar"), "Remote Saved pin"),
    ).toBeVisible();
  } finally {
    await fixture.close();
  }
});

test("peer teams retain members and allow create, edit, dissolve and shared chat on the owner", async ({
  page,
  context,
}) => {
  const { fixture, sidebar } = await setup(context, page);
  try {
    await teamMenu(page, sidebar, "Remote Saved team", "Edit team");
    const form = page.getByRole("form", { name: "Edit team", exact: true });
    await form.getByLabel("Team name").fill("Remote Team renamed");
    await form.getByRole("button", { name: "Save team", exact: true }).click();
    await expect(
      sidebar.getByRole("button", { name: /Remote Team renamed/ }).first(),
    ).toBeVisible();
    expect(fixture.calls.remote[0].body).toMatchObject({
      action: "save",
      path: sourcePath("remote"),
      team_id: "team",
      members: ["member-a", "member-b"],
      expected_revision: 8,
    });
    await teamMenu(page, sidebar, "Remote Team renamed", "Open shared chat");
    await expect
      .poll(() =>
        fixture.calls.remote.some((call) => call.body.action === "radio"),
      )
      .toBe(true);
    await expect(
      page.locator('iframe[title="Studio on Remote"]'),
    ).toBeVisible();
    await teamMenu(page, sidebar, "Remote Team renamed", "Dissolve team");
    await page
      .getByRole("form", { name: "Dissolve team", exact: true })
      .getByRole("button", { name: "Dissolve", exact: true })
      .click();
    await expect(
      sidebar.getByRole("button", { name: /Remote Team renamed/ }),
    ).toHaveCount(0);
    await projectMenu(page, sidebar, "New team");
    const create = page.getByRole("form", { name: "New team", exact: true });
    await create.getByLabel("Team name").fill("Home New team");
    await create.getByLabel("Local Root A", { exact: true }).check();
    await create.getByLabel("Local Root B", { exact: true }).check();
    await expect(
      create.getByLabel("Remote Root A", { exact: true }),
    ).toHaveCount(0);
    await create
      .getByRole("button", { name: "Save team", exact: true })
      .click();
    await expect(
      sidebar.getByRole("button", { name: "Home New team", exact: true }),
    ).toBeVisible();
    expect(fixture.calls.local[0].body).toMatchObject({
      action: "save",
      path: sourcePath("local"),
      members: ["root-a", "root-b"],
      expected_revision: 4,
    });
  } finally {
    await fixture.close();
  }
});

test("conversion from a peer team to a subagent keeps the owner and asks for confirmation", async ({
  page,
  context,
}) => {
  const { fixture, sidebar } = await setup(context, page);
  try {
    await listMenu(page, sidebar, "Expand all projects");
    await drag(
      page,
      chat(sidebar, "Remote Team A").locator("[data-chat]"),
      chat(sidebar, "Remote Root A").locator("[data-chat]"),
    );
    await expect(
      page.getByRole("dialog", { name: "Make chat a subagent", exact: true }),
    ).toBeVisible();
    expect(fixture.calls).toEqual({ local: [], remote: [] });
    await page
      .getByRole("button", { name: "Make subagent", exact: true })
      .click();
    await expect(chat(sidebar, "Remote Team A")).toHaveCount(0);
    expect(fixture.calls.remote[0].body).toMatchObject({
      action: "convert",
      path: sourcePath("remote"),
      member: "member-a",
      target: "root-a",
      expected_revision: 8,
    });
    expect(
      fixture.local.snapshot.threads.find((row) => row.id === "member-a")
        .isLead,
    ).toBe(true);
    await chat(sidebar, "Remote Root A").locator("[data-chat]").click();
    await expect(
      page.locator('iframe[title="Studio on Remote"]'),
    ).toBeVisible();
  } finally {
    await fixture.close();
  }
});

test("project rename, compact, folders, account, new chats, shared chats and directory changes use the owner", async ({
  page,
  context,
}) => {
  const { fixture, sidebar } = await setup(context, page);
  try {
    await projectMenu(page, sidebar, "Show more");
    await expect(sidebar.locator(".project-show-all")).toHaveText("Show less");
    await projectMenu(page, sidebar, "Rename project");
    await page
      .getByRole("textbox", { name: "Project name", exact: true })
      .fill("Home Renamed");
    await page.getByRole("button", { name: "Save name", exact: true }).click();
    await expect(button(sidebar, "Home Renamed")).toBeVisible();
    expect(fixture.calls.local[0].body).toMatchObject({
      action: "rename",
      path: sourcePath("local"),
      expected_revision: 3,
    });
    await button(sidebar, "Home Renamed").hover();
    await button(sidebar, "Options for project Home Renamed").click();
    await page.getByRole("menuitem", { name: "Folders", exact: true }).click();
    const local = page.frameLocator('iframe[title="Studio on This computer"]');
    await expect(
      local.getByRole("dialog", { name: "Project settings", exact: true }),
    ).toBeVisible();
    await local.getByRole("button", { name: "Close", exact: true }).click();
    await button(sidebar, "Home Renamed").hover();
    await button(sidebar, "Options for project Home Renamed").click();
    await page
      .getByRole("menuitem", { name: "Project account", exact: true })
      .click();
    await expect(local.locator(".project-settings-path")).toHaveText(
      sourcePath("local"),
    );
    await local
      .getByRole("group", {
        name: "Accounts shown first for this project",
        exact: true,
      })
      .getByRole("button", { name: "Select account Second", exact: true })
      .click();
    await local
      .getByRole("group", {
        name: "Default account for new chats",
        exact: true,
      })
      .getByRole("button", { name: "Select account Second", exact: true })
      .click();
    await local
      .getByRole("button", { name: "Save accounts", exact: true })
      .click();
    await expect
      .poll(() => fixture.local.snapshot.runtime.projects[0].accountKey)
      .toBe("secondary");
    expect(fixture.remote.snapshot.runtime.projects[0].accountKey).toBe(
      "default",
    );
    await sidebar
      .getByRole("button", { name: /^Remote Empty/ })
      .first()
      .hover();
    await button(sidebar, "New chat in folder Remote Empty").click();
    await expect
      .poll(() =>
        fixture.calls.remote.some((call) => call.path === "/api/leads"),
      )
      .toBe(true);
    expect(
      fixture.calls.remote.find((call) => call.path === "/api/leads").body,
    ).toMatchObject({ cwd: sourcePath("remote"), project_folder: "empty" });
    await expect(
      page.locator('iframe[title="Studio on Remote"]'),
    ).toBeVisible();
    await menu(page, sidebar, "Remote Root B", "Change project directory");
    const remote = page.frameLocator('iframe[title="Studio on Remote"]');
    const picker = remote.getByRole("dialog", {
      name: "Choose project folder",
      exact: true,
    });
    await picker
      .getByRole("button", { name: "New directory", exact: true })
      .click();
    await picker
      .getByRole("button", { name: "Add project", exact: true })
      .click();
    await expect
      .poll(
        () =>
          fixture.remote.snapshot.threads.find((row) => row.id === "root-b")
            .cwd,
      )
      .toBe("/remote/new-directory");
    await button(sidebar, "Home Renamed").hover();
    await button(sidebar, "Options for project Home Renamed").click();
    await page
      .getByRole("menuitem", { name: "New shared chat", exact: true })
      .click();
    await expect(
      local.getByRole("form", { name: "Create shared chat", exact: true }),
    ).toBeVisible();
    await local
      .getByRole("textbox", { name: "Chat name", exact: true })
      .fill("Home New shared chat");
    await local.getByRole("button", { name: "Create", exact: true }).click();
    await expect
      .poll(() =>
        fixture.calls.local.some((call) => call.body.radio_action === "create"),
      )
      .toBe(true);
    await expect(
      button(sidebar, "Home New shared chat Codex, MAC"),
    ).toBeVisible();
  } finally {
    await fixture.close();
  }
});

test("chat pin, rename, archive, restore, delete, mark unread and prefetch use the owning server", async ({
  page,
  context,
}) => {
  const { fixture, sidebar } = await setup(context, page);
  try {
    const initialLocal = structuredClone(fixture.local.snapshot.threads);
    await chat(sidebar, "Remote Read answer").locator("[data-chat]").click();
    await chat(sidebar, "Remote Root A").locator("[data-chat]").hover();
    await expect
      .poll(() => fixture.reads.remote.some((path) => path.includes("root-a")))
      .toBe(true);
    await menu(page, sidebar, "Remote Root A", "Pin");
    await expect(
      chat(sidebar, "Remote Root A").locator(".chat-pin"),
    ).toBeVisible();
    await menu(page, sidebar, "Remote Root A", "Unpin");
    await menu(page, sidebar, "Remote Root A", "Rename");
    await chat(sidebar, "Remote Root A")
      .getByLabel("Chat name")
      .fill("Remote Renamed");
    await button(chat(sidebar, "Remote Root A"), "Save name").click();
    await expect(chat(sidebar, "Remote Renamed")).toBeVisible();
    await menu(page, sidebar, "Remote Read answer", "Mark unread");
    await expect(
      chat(sidebar, "Remote Read answer").locator('[role="img"]'),
    ).toHaveAttribute("aria-label", /Unread result/);
    await menu(page, sidebar, "Remote Renamed", "Archive");
    await expect(chat(sidebar, "Remote Renamed")).toHaveCount(0);
    await listMenu(page, sidebar, "Show archived chats");
    await menu(page, sidebar, "Remote Renamed", "Restore chat");
    await listMenu(page, sidebar, "Show active chats");
    await expect(chat(sidebar, "Remote Renamed")).toBeVisible();
    await menu(page, sidebar, "Remote Renamed", "Delete");
    const remote = page.frameLocator('iframe[title="Studio on Remote"]');
    await remote
      .getByRole("button", { name: "Delete chat", exact: true })
      .click();
    await expect(chat(sidebar, "Remote Renamed")).toHaveCount(0);
    expect(fixture.local.snapshot.threads).toEqual(initialLocal);
    expect(fixture.calls.local).toEqual([]);
    const mutations = fixture.calls.remote;
    expect(mutations.map((call) => call.path)).toEqual([
      "/api/organization",
      "/api/organization",
      "/api/rename",
      "/api/organization",
      "/api/organization",
      "/api/organization",
      "/api/conversation/delete",
    ]);
    expect(mutations.map((call) => call.body.id)).toEqual([
      "root-a",
      "root-a",
      "root-a",
      "read",
      "root-a",
      "root-a",
      "root-a",
    ]);
  } finally {
    await fixture.close();
  }
});

test("folder create, nested create, rename, remove and chat membership use original source folders", async ({
  page,
  context,
}) => {
  const { fixture, sidebar } = await setup(context, page);
  try {
    await folderMenu(page, sidebar, "Remote Parent", "New subfolder");
    await page
      .getByRole("textbox", { name: "Folder name", exact: true })
      .fill("Remote New nested");
    await page
      .getByRole("button", { name: "Create folder", exact: true })
      .click();
    await expect(
      sidebar.getByRole("button", { name: /Remote New nested/ }).first(),
    ).toBeVisible();
    expect(fixture.calls.remote[0].body).toMatchObject({
      action: "add_folder",
      path: sourcePath("remote"),
      parent_id: "parent",
      expected_revision: 9,
    });
    await folderMenu(page, sidebar, "Remote New nested", "Rename folder");
    await page
      .getByRole("textbox", { name: "Folder name", exact: true })
      .fill("Remote Folder renamed");
    await page.getByRole("button", { name: "Save name", exact: true }).click();
    await expect(
      sidebar.getByRole("button", { name: /Remote Folder renamed/ }).first(),
    ).toBeVisible();
    await menu(page, sidebar, "Remote Root A", "Move to folder");
    await page
      .getByLabel("Move to folder", { exact: true })
      .selectOption({ label: "Remote Parent / Remote Folder renamed" });
    await page.getByRole("button", { name: "Move chat", exact: true }).click();
    await expect(
      chat(sidebar, "Remote Root A").locator(
        "xpath=ancestor::*[@data-folder-id][1]",
      ),
    ).toBeVisible();
    await menu(page, sidebar, "Remote Root A", "Move to folder");
    await page
      .getByLabel("Move to folder", { exact: true })
      .selectOption({ label: "Home project" });
    await page.getByRole("button", { name: "Move chat", exact: true }).click();
    await folderMenu(
      page,
      sidebar,
      "Remote Folder renamed",
      "Remove empty folder",
    );
    await expect(
      sidebar.getByRole("button", { name: /Remote Folder renamed/ }),
    ).toHaveCount(0);
    await projectMenu(page, sidebar, "New chat folder");
    await page
      .getByRole("textbox", { name: "Folder name", exact: true })
      .fill("Home New folder");
    await page
      .getByRole("button", { name: "Create folder", exact: true })
      .click();
    await expect(
      sidebar.getByRole("button", { name: "Home New folder", exact: true }),
    ).toBeVisible();
    expect(fixture.calls.local).toHaveLength(1);
    expect(fixture.calls.local[0].body).toMatchObject({
      action: "add_folder",
      path: sourcePath("local"),
      expected_revision: 3,
    });
    expect(
      fixture.local.snapshot.runtime.projects[0].folders.find(
        (folder) => folder.id === "parent",
      ).name,
    ).toBe("Local Parent");
  } finally {
    await fixture.close();
  }
});

test("project and pin keyboard order persist on the home server without changing remote order", async ({
  page,
  context,
}) => {
  test.setTimeout(90000);
  await page.setViewportSize({ width: 1440, height: 1100 });
  const fixture = await sidebarParityFixture(context);
  const original = fixture.local.snapshot.runtime.projects[0];
  fixture.local.snapshot.runtime.projects.push({
    ...structuredClone(original),
    id: "other-project",
    name: "Other project",
    path: "/same/other",
    folders: [],
    peerTeams: [],
  });
  fixture.local.snapshot.runtime.sidebarOrder.groups.projects.push(
    "/same/other",
  );
  const remoteOrder = structuredClone(
    fixture.remote.snapshot.runtime.sidebarOrder,
  );
  try {
    const sidebar = await fixture.open(page);
    await expect(button(sidebar, "Other project")).toBeVisible();
    await button(sidebar, "Other project").focus();
    await page.keyboard.press("Alt+ArrowUp");
    await expect
      .poll(() => fixture.local.snapshot.runtime.sidebarOrder.groups.projects)
      .toEqual(["/same/other", sourcePath("local")]);
    await menu(page, sidebar, "Local Root A", "Pin");
    const pin = chat(sidebar, "Local Root A").locator("[data-chat]");
    await expect(
      chat(sidebar, "Local Root A").locator(".chat-pin"),
    ).toBeVisible();
    await pin.focus();
    await page.keyboard.press("Alt+ArrowUp");
    await expect
      .poll(
        () =>
          fixture.calls.local.filter((call) => call.body.action === "reorder")
            .length,
      )
      .toBe(2);
    await expect(sidebar.getByRole("status")).toHaveText(
      "Moved to position 2 of 3",
    );
    await pin.focus();
    await page.keyboard.press("Alt+ArrowUp");
    await expect
      .poll(
        () =>
          fixture.calls.local.filter((call) => call.body.action === "reorder")
            .length,
      )
      .toBe(3);
    const key = JSON.stringify(["chats", sourcePath("local"), true, false]);
    expect(fixture.local.snapshot.runtime.sidebarOrder.groups[key]).toEqual([
      "root-a",
      "overlap",
    ]);
    expect(fixture.remote.snapshot.runtime.sidebarOrder).toEqual(remoteOrder);
    expect(fixture.calls.remote).toEqual([]);
    await page.reload();
    const restored = page.locator("#sidebar");
    await expect(
      chat(restored, "Local Root A").locator(".chat-pin"),
    ).toBeVisible();
    await expect
      .poll(() =>
        restored
          .locator(".sidebar-project")
          .evaluateAll((rows) =>
            rows.map(
              (row) =>
                row.querySelector(".project-tree-toggle span")?.textContent,
            ),
          ),
      )
      .toEqual(["Other project", "Home project"]);
    const pins = await restored
      .locator(".sidebar-row")
      .filter({ has: page.locator(".chat-pin") })
      .allTextContents();
    expect(
      pins.findIndex((text) => text.includes("Local Root A")),
    ).toBeLessThan(pins.findIndex((text) => text.includes("Local Saved pin")));
  } finally {
    await fixture.close();
  }
});

test("global search opens its source chat, and toolbar actions use the active server", async ({
  page,
  context,
}) => {
  const { fixture, sidebar } = await setup(context, page);
  try {
    await button(sidebar, "Search chats").click();
    const search = page.getByRole("dialog", {
      name: "Search all servers",
      exact: true,
    });
    await search.getByLabel("Search all messages").fill("saved history");
    await search.getByRole("button", { name: "Search", exact: true }).click();
    await expect(search.locator(".server-search-result")).toHaveCount(2);
    for (const owner of ["local", "remote"])
      expect(
        fixture.reads[owner].some((path) =>
          path.includes("/api/search?q=saved"),
        ),
      ).toBe(true);
    await search
      .locator(".server-search-result")
      .filter({ hasText: "Remote: saved history" })
      .click();
    const remote = page.frameLocator('iframe[title="Studio on Remote"]');
    await expect(
      remote.getByRole("heading", { name: "Remote Saved pin", exact: true }),
    ).toBeVisible();
    await button(sidebar, "New shared chat").click();
    await expect(
      remote.getByRole("form", { name: "Create shared chat", exact: true }),
    ).toBeVisible();
    await remote.getByRole("button", { name: "Close", exact: true }).click();
    await button(sidebar, "New chat").click();
    await expect
      .poll(() =>
        fixture.calls.remote.some((call) => call.path === "/api/leads"),
      )
      .toBe(true);
    expect(
      fixture.calls.remote.find((call) => call.path === "/api/leads").body,
    ).toMatchObject({ cwd: sourcePath("remote") });
    expect(fixture.calls.local).toEqual([]);
  } finally {
    await fixture.close();
  }
});
