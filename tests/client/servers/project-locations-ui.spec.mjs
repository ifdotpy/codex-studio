import { test, expect } from "../playwright.mjs";
import { fixture } from "./multi-server-fixture.mjs";

test.setTimeout(180000);

async function projectFixtures(offline = false) {
  const leads = [];
  const browses = [];
  const registrations = [];
  const project = {
    id: "project:logical",
    path: "project:logical",
    name: "attar",
    homeServerId: "local",
    locations: [
      {
        serverId: "local",
        path: "/Projects/chrompile",
        projectId: "/Projects/chrompile",
      },
      {
        serverId: "remote",
        path: "/Projects/attar",
        projectId: "/Projects/attar",
      },
    ],
  };
  const remote = await fixture("Remote", true, {
    handle({ url, body, request, json, snapshot }) {
      if (url.pathname !== "/api/leads") return false;
      leads.push({ ...body, token: request.headers["x-canvas-token"] });
      json({
        ...snapshot.threads[0],
        id: body.id,
        cwd: body.cwd,
        projectId: body.project_id,
        projectServerId: body.project_server_id,
      });
      return true;
    },
  });
  const local = await fixture("Local", false, {
    handle({ url, body, request, json, snapshot }) {
      if (url.pathname === "/api/leads") {
        leads.push({
          ...body,
          server: "local",
          token: request.headers["x-canvas-token"],
        });
        json({
          ...snapshot.threads[0],
          id: body.id,
          cwd: body.cwd,
          accountKey: "logical-account",
          projectId: body.project_id,
          projectServerId: body.project_server_id,
        });
        return true;
      }
      if (url.pathname === "/api/projects") {
        if (body.path) {
          registrations.push(body);
          json({
            outcome: "applied",
            requestId: body.request_id,
            server: body.server,
            action: "add_location",
            projectId: "project:new",
            value: {},
          });
        } else json({ items: snapshot.runtime.projects });
        return true;
      }
      if (url.pathname === "/api/project-locations") {
        json({ gitHead: "12345678", matches: [] });
        return true;
      }
      if (url.pathname !== "/api/directories") return false;
      browses.push(Object.fromEntries(url.searchParams));
      const path =
        url.searchParams.get("path") ||
        (url.searchParams.get("server") ? "/remote/home" : "/local/home");
      json({
        path,
        parent: "/",
        directories: [{ name: "attar", path: path + "/attar" }],
      });
      return true;
    },
  });
  project.accountKey = "logical-account";
  local.snapshot.runtime.projects = [
    project,
    {
      id: "/Projects/chrompile",
      path: "/Projects/chrompile",
      name: "attar",
      accountKey: "default",
      homeServerId: "local",
      locations: [
        {
          serverId: "local",
          path: "/Projects/chrompile",
          projectId: "/Projects/chrompile",
        },
      ],
      projectAliases: [
        { serverId: "local", projectId: project.id, name: "attar" },
      ],
    },
  ];
  Object.assign(local.snapshot.threads[0], {
    cwd: "/Projects/chrompile",
    projectId: project.id,
    projectServerId: "local",
    updated: Date.now() / 1000 - 1,
  });
  remote.snapshot.runtime.projects = [
    {
      id: "/Projects/attar",
      path: "/Projects/attar",
      name: "attar",
      homeServerId: "remote",
      projectAliases: [
        { serverId: "local", projectId: project.id, name: "attar" },
      ],
      locations: [
        {
          serverId: "remote",
          path: "/Projects/attar",
          projectId: "/Projects/attar",
        },
      ],
    },
  ];
  Object.assign(remote.snapshot.threads[0], {
    cwd: "/Projects/attar",
    projectId: project.id,
    projectServerId: "local",
    updated: Date.now() / 1000,
  });
  local.discoverPeer(remote, "paired", offline ? "unreachable" : "reachable");
  return { leads, browses, registrations, project, local, remote };
}

async function openFixtures(page, context, local, remote) {
  await context.addInitScript(
    ({ destination, source }) => {
      const native = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const request = new Request(input, init);
        const url = new URL(request.url);
        if (url.origin !== source) return native(request);
        const bytes = await request.clone().arrayBuffer();
        return native(destination + url.pathname + url.search, {
          method: request.method,
          headers: request.headers,
          ...(bytes.byteLength ? { body: bytes } : {}),
          signal: request.signal,
          redirect: "error",
          credentials: "omit",
        });
      };
    },
    { destination: remote.origin, source: remote.invitation.origin },
  );
  await page.goto(local.origin + "/?studio-navigation=combined");
}

test("project folders, remote browse, and New chat server choice", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(180000);
  const { leads, browses, registrations, project, local, remote } =
    await projectFixtures();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await openFixtures(page, context, local, remote);
    const group = page.locator(
      '.server-sidebar [data-project-path="project:logical"]',
    );
    await expect(group.getByText("Remote chat", { exact: true })).toBeVisible({
      timeout: 20000,
    });
    await expect(group.getByText("Local chat", { exact: true })).toBeVisible();
    await group
      .getByRole("button", { name: "New chat in attar", exact: true })
      .click();
    const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
    await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(dialog).not.toBeVisible();
    expect(leads).toHaveLength(0);
    await group
      .getByRole("button", { name: "New chat in attar", exact: true })
      .click();
    await expect(
      dialog.getByRole("button", { name: "Start chat", exact: true }),
    ).toHaveAttribute("data-variant", "filled");
    const remoteCard = dialog.getByRole("button", {
      name: /Remote.*Active.*\/Projects\/attar/,
    });
    await expect(remoteCard).toHaveAttribute("aria-pressed", "true");
    const localCard = dialog.getByRole("button", {
      name: /Local.*Active.*chrompile/,
    });
    await localCard.click();
    await expect(localCard).toHaveAttribute("aria-pressed", "true");
    await expect(remoteCard).toHaveAttribute("aria-pressed", "false");
    await remoteCard.click();
    await expect(remoteCard).toHaveAttribute("aria-pressed", "true");
    await page.screenshot({
      animations: "disabled",
      path: testInfo.outputPath("new-chat-server-cards.png"),
    });
    await dialog
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    await expect.poll(() => leads.length).toBe(1);
    expect(leads[0].cwd).toBe("/Projects/attar");
    expect(leads[0].project_id).toBe(project.id);
    expect(leads[0].project_server_id).toBe("local");
    expect(leads[0].token).toBeUndefined();
    await group
      .getByRole("button", { name: "Options for project attar", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Folders", exact: true }).click();
    const frame = page.frameLocator('iframe[title="Studio on Local"]');
    const folders = frame.getByRole("dialog", {
      name: "Project settings",
      exact: true,
    });
    await expect(
      folders.getByText("/Projects/chrompile", { exact: true }),
    ).toBeVisible();
    await expect(
      folders.getByText("/Projects/attar", { exact: true }),
    ).toBeVisible();
    await expect(folders.getByText("HEAD 12345678")).toHaveCount(2);
    await page.screenshot({
      animations: "disabled",
      path: testInfo.outputPath("project-folders.png"),
    });
    await folders.getByRole("button", { name: "Close", exact: true }).click();
    await page
      .locator(".server-sidebar")
      .getByRole("button", { name: "Add project", exact: true })
      .click();
    const add = frame.getByRole("dialog", { name: "Add project", exact: true });
    await add.getByLabel("Server", { exact: true }).click();
    await frame.getByRole("option", { name: "Remote", exact: true }).click();
    await expect(add.getByLabel("Folder path", { exact: true })).toHaveValue(
      "/remote/home",
    );
    await page.screenshot({
      animations: "disabled",
      path: testInfo.outputPath("remote-folder-browser.png"),
    });
    await add.getByRole("button", { name: /attar/ }).click();
    const footer = await add
      .getByRole("button", { name: "Add project", exact: true })
      .boundingBox();
    const frameBounds = await page
      .locator('iframe[title="Studio on Local"]')
      .boundingBox();
    expect(footer.y + footer.height).toBeLessThanOrEqual(
      frameBounds.y + frameBounds.height,
    );
    await add.getByRole("button", { name: "Add project", exact: true }).click();
    await expect.poll(() => registrations.length).toBe(1);
    expect(registrations[0].server).toBe("remote");
    expect(registrations[0].path).toBe("/remote/home/attar");
    expect(browses.some((row) => row.server === "remote" && !row.path)).toBe(
      true,
    );
    expect(errors).toEqual([]);
    await page.screenshot({
      animations: "disabled",
      path: testInfo.outputPath("project-locations.png"),
    });
  } finally {
    await local.close();
    await remote.close();
  }
});

test("bound local New chat lets the logical project choose its account", async ({
  page,
  context,
}) => {
  const { local, remote, leads } = await projectFixtures();
  try {
    await openFixtures(page, context, local, remote);
    const group = page.locator(
      '.server-sidebar [data-project-path="project:logical"]',
    );
    await expect(group.getByText("Remote chat", { exact: true })).toBeVisible({
      timeout: 20000,
    });
    await group
      .getByRole("button", { name: "New chat in attar", exact: true })
      .click();
    const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
    await dialog
      .getByRole("button", { name: /Local.*Active.*chrompile/ })
      .click();
    await dialog
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    await expect.poll(() => leads.length).toBe(1);
    expect(leads[0].project_id).toBe("project:logical");
    expect(leads[0].account_key).toBeUndefined();
  } finally {
    await local.close();
    await remote.close();
  }
});

test("project folder menu keeps the SidebarRow icon size", async ({
  page,
  context,
}) => {
  const { local, remote } = await projectFixtures();
  try {
    await openFixtures(page, context, local, remote);
    const group = page.locator(
      '.server-sidebar [data-project-path="project:logical"]',
    );
    await expect(group.getByText("Remote chat", { exact: true })).toBeVisible({
      timeout: 20000,
    });
    await group
      .getByRole("button", { name: "Options for project attar", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Folders", exact: true }).click();
    const frame = page.frameLocator('iframe[title="Studio on Local"]');
    const button = frame.getByRole("button", {
      name: "Options for folder /Projects/chrompile",
      exact: true,
    });
    await expect(button).toBeVisible();
    const icon = await button.locator("svg").boundingBox();
    expect(icon.width).toBe(16);
    expect(icon.height).toBe(16);
  } finally {
    await local.close();
    await remote.close();
  }
});

test("offline paired server is disabled in all project choices", async ({
  page,
  context,
}) => {
  const { local, remote } = await projectFixtures(true);
  try {
    await openFixtures(page, context, local, remote);
    const group = page.locator(
      '.server-sidebar [data-project-path="project:logical"]',
    );
    await expect(group.getByText("Remote chat", { exact: true })).toBeVisible({
      timeout: 20000,
    });
    await group
      .getByRole("button", { name: "New chat in attar", exact: true })
      .click();
    const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
    await expect(
      dialog.getByRole("button", { name: /Remote.*Offline.*attar/ }),
    ).toBeDisabled();
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
    await page
      .locator(".server-sidebar")
      .getByRole("button", { name: "Add project", exact: true })
      .click();
    const frame = page.frameLocator('iframe[title="Studio on Local"]');
    const add = frame.getByRole("dialog", { name: "Add project", exact: true });
    await add.getByLabel("Server", { exact: true }).click();
    await expect(
      frame.getByRole("option", { name: "Remote (Offline)", exact: true }),
    ).toHaveAttribute("data-combobox-disabled", "true");
    await add.getByRole("button", { name: "Close", exact: true }).click();
    await group
      .getByRole("button", { name: "Options for project attar", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Folders", exact: true }).click();
    const folders = frame.getByRole("dialog", {
      name: "Project settings",
      exact: true,
    });
    await folders
      .getByRole("button", { name: "Add folder", exact: true })
      .click();
    await folders.getByLabel("Server", { exact: true }).click();
    await expect(
      frame.getByRole("option", { name: "Remote (Offline)", exact: true }),
    ).toHaveAttribute("data-combobox-disabled", "true");
  } finally {
    await local.close();
    await remote.close();
  }
});

for (const variant of ["light", "dark", "mobile"]) {
  test(`project sidebar and New chat cards ${variant}`, async ({
    page,
    context,
  }, testInfo) => {
    const { local, remote } = await projectFixtures();
    try {
      await page.setViewportSize(
        variant === "mobile"
          ? { width: 390, height: 844 }
          : { width: 1280, height: 720 },
      );
      await page.emulateMedia({
        colorScheme: variant === "dark" ? "dark" : "light",
      });
      await openFixtures(page, context, local, remote);
      if (variant === "mobile")
        await page
          .getByRole("button", { name: "Servers and chats", exact: true })
          .click();
      const group = page.locator(
        '.server-sidebar [data-project-path="project:logical"]',
      );
      await expect(
        group
          .getByText("Remote chat", { exact: true })
          .locator("..")
          .locator(".chat-server-line"),
      ).toBeVisible();
      await expect(page.locator("html")).toHaveAttribute(
        "data-mantine-color-scheme",
        variant === "dark" ? "dark" : "light",
      );
      await page.screenshot({
        animations: "disabled",
        path: testInfo.outputPath(`sidebar-${variant}.png`),
      });
      await group
        .getByRole("button", { name: "New chat in attar", exact: true })
        .click();
      const dialog = page.getByRole("dialog", {
        name: "New chat",
        exact: true,
      });
      await expect(
        dialog.getByRole("button", { name: "Start chat", exact: true }),
      ).toBeVisible();
      await page.screenshot({
        animations: "disabled",
        path: testInfo.outputPath(`new-chat-${variant}.png`),
      });
    } finally {
      await local.close();
      await remote.close();
    }
  });
}
