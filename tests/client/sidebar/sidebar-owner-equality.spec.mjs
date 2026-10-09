import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

const scope = "/same/state";
const path = "/same/project";
const otherPath = "/other/project";
const folderKey = (id) => JSON.stringify([path, "folder", id]);
const teamKey = JSON.stringify([path, "team", "team"]);
const compactKey = `codex-project-compact:${scope}`;
const treeKey = `codex-project-tree:${scope}`;
const orderKey = `codex-sidebar-order:${scope}`;
const compact = { [path]: false, [otherPath]: true };
const tree = {
  [otherPath]: true,
  [folderKey("nested")]: true,
  [teamKey]: true,
};
const order = {
  projects: [otherPath, path],
  [JSON.stringify(["items", path, null])]: [teamKey, folderKey("parent")],
  [JSON.stringify(["items", path, "parent"])]: [folderKey("nested")],
  [JSON.stringify(["chats", path, true, false])]: ["overlap"],
};

async function organization(sidebar) {
  return sidebar.locator("#chat-list").evaluate((list) => {
    const visible = (element) => element.getClientRects().length > 0;
    return {
      projects: [...list.querySelectorAll(".sidebar-project")].map(
        (project) => ({
          path: project.dataset.projectPath,
          name: project.querySelector(
            ":scope > .project-tree-heading .project-tree-toggle span",
          ).textContent,
          expanded: project
            .querySelector(
              ":scope > .project-tree-heading .project-tree-toggle",
            )
            .getAttribute("aria-expanded"),
        }),
      ),
      folders: [...list.querySelectorAll("[data-folder-id]")]
        .filter(visible)
        .map((folder) => ({
          id: folder.dataset.folderId,
          parent:
            folder.parentElement.closest("[data-folder-id]")?.dataset
              .folderId || null,
          name: folder.querySelector(
            ":scope > .project-tree-heading .project-tree-toggle span",
          ).textContent,
          expanded: folder
            .querySelector(
              ":scope > .project-tree-heading .project-tree-toggle",
            )
            .getAttribute("aria-expanded"),
        })),
      teams: [...list.querySelectorAll("[data-peer-team]")]
        .filter(visible)
        .map((team) => ({
          id: team.dataset.peerTeam,
          name: team.querySelector(".peer-team-toggle span").textContent,
          expanded: team
            .querySelector(".peer-team-toggle")
            .getAttribute("aria-expanded"),
        })),
      chats: [...list.querySelectorAll(".chat-row")]
        .filter(visible)
        .map((chat) => ({
          id: chat.dataset.chat || null,
          name: chat.querySelector("strong").textContent,
          pinned: !!chat.querySelector(".chat-pin"),
          alias: chat.querySelector(".chat-server-line").textContent,
          folder: chat.closest("[data-folder-id]")?.dataset.folderId || null,
          team: chat.closest("[data-peer-team]")?.dataset.peerTeam || null,
        })),
      compact: [...list.querySelectorAll(".project-show-all")]
        .filter(visible)
        .map((button) => button.textContent),
    };
  });
}

function seedFixture(local) {
  const base = local.snapshot.threads[0];
  const chat = (id, name, extra = {}) => ({
    ...base,
    id,
    rootId: id,
    name,
    created: 1,
    updated: 1,
    ...extra,
  });
  const teams = [
    {
      id: "team",
      name: "Saved team",
      projectPath: path,
      members: ["member-a", "member-b"],
    },
  ];
  local.setChats([
    chat("overlap", "Saved pin", { pinned: true }),
    chat("nested-chat", "Nested chat", { projectFolder: "nested" }),
    chat("member-a", "Team member A"),
    chat("member-b", "Team member B"),
    chat("archived-chat", "Archived chat", { archived: true }),
  ]);
  Object.assign(local.snapshot.runtime, {
    projectOrganizationVersion: 1,
    peerTeamsVersion: 1,
    sidebarOrder: { revision: 3, groups: order },
    peerTeams: teams,
    projects: [
      {
        ...local.snapshot.runtime.projects[0],
        path,
        name: "Project A",
        folders: [
          { id: "parent", name: "Parent", parentId: null },
          { id: "nested", name: "Nested", parentId: "parent" },
        ],
        organizationRevision: 2,
        peerTeams: teams,
        peerTeamsRevision: 1,
      },
      { path: otherPath, id: otherPath, name: "Project B", folders: [] },
    ],
    rooms: [
      {
        id: "shared",
        name: "Saved shared chat",
        projectPath: path,
        members: ["member-a", "member-b"],
        radio: { direct: true },
      },
    ],
  });
}

test("the owner adapter preserves saved organization in standalone and classic server views", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(90000);
  await page.setViewportSize({ width: 1440, height: 900 });
  const local = await fixture("Local");
  const remote = await fixture("Remote", true);
  seedFixture(local);
  try {
    await context.addInitScript(
      ({
        compactKey,
        treeKey,
        orderKey,
        compact,
        tree,
        order,
        origin,
        destination,
      }) => {
        window.__sidebarSnapshots = [];
        window.addEventListener("message", (event) => {
          if (event.data?.kind === "studio-server-navigation")
            window.__sidebarSnapshots.push(event.data);
        });
        const server = new URLSearchParams(location.search).get(
          "studio-server",
        );
        if (!server || server === "local") {
          for (const [key, value] of [
            [compactKey, compact],
            [treeKey, tree],
            [orderKey, order],
          ]) {
            localStorage.setItem(key, JSON.stringify(value));
          }
        }
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
        compactKey,
        treeKey,
        orderKey,
        compact,
        tree,
        order,
        origin: remote.invitation.origin,
        destination: remote.origin,
      },
    );
    await page.goto(local.origin);
    const standalone = page.locator("#sidebar");
    await expect(standalone.locator(".chat-pin")).toBeVisible();
    await expect(
      standalone.getByRole("button", { name: "Saved team", exact: true }),
    ).toBeVisible();
    await expect(standalone.locator('[data-folder-id="nested"]')).toBeVisible();
    await expect(standalone.locator('[data-chat="nested-chat"]')).toHaveCount(
      0,
    );
    const before = await organization(standalone);
    expect(before.projects.map((row) => row.name)).toEqual([
      "Project B",
      "Project A",
    ]);
    expect(before.projects[0].expanded).toBe("false");
    expect(before.folders).toEqual([
      { id: "parent", parent: null, name: "Parent", expanded: "true" },
      { id: "nested", parent: "parent", name: "Nested", expanded: "false" },
    ]);
    expect(before.teams).toEqual([
      { id: "team", name: "Saved team", expanded: "false" },
    ]);
    expect(before.chats.map((row) => row.name)).toEqual([
      "Saved shared chat",
      "Saved pin",
    ]);
    expect(before.chats.find((row) => row.id === "overlap").pinned).toBe(true);
    expect(before.compact).toEqual(["Show less"]);
    await standalone.screenshot({
      path: testInfo.outputPath("standalone-saved-sidebar.png"),
    });
    local.discoverPeer(remote);
    await page.reload();
    await expect(page.getByLabel("Studio server", { exact: true })).toHaveValue(
      "local",
    );
    await expect(page.locator(".server-sidebar")).toHaveCount(0);
    const localFrame = page.frameLocator(
      'iframe[title="Studio on This computer"]',
    );
    const classic = localFrame.locator("#sidebar");
    await expect(classic.locator(".chat-pin")).toBeVisible();
    await expect(
      classic.getByRole("button", { name: "Saved team", exact: true }),
    ).toBeVisible();
    expect(await organization(classic)).toEqual(before);
    const frameSnapshot = () =>
      page.evaluate(
        () =>
          window.__sidebarSnapshots.findLast(
            (message) =>
              message.serverId === "local" && message.navigation?.sidebar,
          )?.navigation.sidebar || null,
      );
    await expect.poll(frameSnapshot).not.toBeNull();
    const published = await frameSnapshot();
    expect(published.stateDir).toBe(scope);
    expect(published.compact).toEqual(compact);
    expect(published.collapsed).toEqual(tree);
    expect(published.sidebarOrder).toEqual({ revision: 3, groups: order });
    expect(
      published.projects.find((project) => project.path === path).folders,
    ).toEqual([
      { id: "parent", name: "Parent", parentId: null },
      { id: "nested", name: "Nested", parentId: "parent" },
    ]);
    expect(published.peerTeams.map((team) => team.id)).toEqual(["team"]);
    expect(published.threads.find((chat) => chat.id === "overlap").pinned).toBe(
      true,
    );
    expect(
      published.threads.find((chat) => chat.id === "archived-chat").archived,
    ).toBe(true);
    expect(published).not.toHaveProperty("token");
    await classic.screenshot({
      path: testInfo.outputPath("classic-saved-sidebar.png"),
    });
    await classic.getByRole("button", { name: "Project list options" }).click();
    await localFrame
      .getByRole("menuitem", { name: "Show archived chats", exact: true })
      .click();
    await expect(classic.locator('[data-chat="archived-chat"]')).toBeVisible();
    await expect(classic.locator('[data-chat="overlap"]')).toHaveCount(0);
    expect(
      await localFrame
        .locator("body")
        .evaluate(
          ({ ownerDocument }, keys) =>
            Object.fromEntries(
              keys.map((key) => [
                key,
                JSON.parse(ownerDocument.defaultView.localStorage.getItem(key)),
              ]),
            ),
          [compactKey, treeKey, orderKey],
        ),
    ).toEqual({ [compactKey]: compact, [treeKey]: tree, [orderKey]: order });
    expect(local.writes).toEqual([]);
    expect(remote.writes).toEqual([]);
  } finally {
    await remote.close();
    await local.close();
  }
});
