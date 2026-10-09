import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

const models = ["gpt-6-astra", "gpt-6-luna", "gpt-5.6-sol"].map((model) => ({
  model,
  provider: "codex",
  displayName: model,
  isDefault: model === "gpt-6-astra",
  defaultReasoningEffort: "medium",
  supportedReasoningEfforts: ["low", "medium", "high"].map(
    (reasoningEffort) => ({ reasoningEffort }),
  ),
  serviceTiers: [{ id: "priority" }],
}));
async function setup(system = "Darwin", label = "Local", signed = false) {
  const creations = [];
  const changes = [];
  const modelReads = [];
  const server = await fixture(label, signed, {
    handle({ url, body, request, json, snapshot }) {
      if (url.pathname === "/api/projects") {
        json({ items: snapshot.runtime.projects });
        return true;
      }
      if (url.pathname === "/api/models") {
        modelReads.push(Object.fromEntries(url.searchParams));
        json({ data: label === "Remote" ? [models[2]] : models });
        return true;
      }
      if (url.pathname === "/api/ui-summary") {
        json({
          ready: true,
          busy: false,
          system,
          agentsRunning: 0,
          alerts: [],
          accounts: [
            {
              provider: "codex",
              email: null,
              plan: null,
              status: "ready",
              label: "Fixture",
              isDefault: true,
            },
          ],
          projects: snapshot.runtime.projects,
          chats: snapshot.threads.map((row) => ({
            id: row.id,
            name: row.name,
            path: row.cwd,
            archived: false,
            status: "idle",
            unread: false,
          })),
        });
        return true;
      }
      if (url.pathname === "/api/conversation" && request.method === "POST") {
        changes.push(body);
        json({});
        return true;
      }
      if (url.pathname === "/api/leads") {
        creations.push(body);
        const row = {
          ...snapshot.threads[0],
          id: body.id,
          name: "New chat",
          cwd: body.cwd,
          model: body.model,
          effort: body.effort,
          workspaceMode: body.workspaceMode,
          executionMode: body.workspaceMode === "layr" ? "vm" : "native",
        };
        snapshot.threads.push(row);
        snapshot.runtime.agents = snapshot.threads;
        json(row);
        return true;
      }
      return false;
    },
  });
  return { server, creations, changes, modelReads };
}
async function openNewChat(page, server, classic = false) {
  await page.goto(
    server.origin +
      (classic ? "/?studio-single=1" : "/?studio-navigation=combined"),
  );
  await page
    .locator('[data-project-path="/same/project"] .project-tree-heading')
    .hover();
  await page
    .getByRole("button", { name: "New chat in Local project", exact: true })
    .click();
  const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
  await expect(
    dialog.getByRole("button", { name: "Start chat", exact: true }),
  ).toBeEnabled();
  return dialog;
}
for (const [choice, badge] of [
  ["layr", "LAYR"],
  ["ASIF", "ASIF"],
  ["worktree", "WT"],
]) {
  test(`macOS New chat ${choice} and actual model menus`, async ({ page }) => {
    const { server, creations, changes } = await setup();
    try {
      const dialog = await openNewChat(page, server);
      await expect(
        dialog.getByRole("radio", { name: "layr", exact: true }),
      ).toBeChecked();
      await dialog.getByText(choice, { exact: true }).click();
      await dialog
        .getByRole("button", { name: "Main agent settings", exact: true })
        .click();
      const main = page.getByRole("dialog", {
        name: "Main agent settings",
        exact: true,
      });
      await main.getByRole("option", { name: /Sol/ }).click();
      await main.getByText("High", { exact: true }).click();
      await page.keyboard.press("Escape");
      await dialog
        .getByRole("button", { name: "Subagent defaults", exact: true })
        .click();
      const worker = page.getByRole("dialog", {
        name: "Subagent defaults",
        exact: true,
      });
      await worker.getByRole("option", { name: /Astra/ }).click();
      await worker.getByText("Low", { exact: true }).click();
      await page.keyboard.press("Escape");
      expect(changes).toHaveLength(0);
      await dialog
        .getByRole("button", { name: "Start chat", exact: true })
        .click();
      await expect.poll(() => creations.length).toBe(1);
      expect(creations[0]).toMatchObject({
        workspaceMode: choice === "ASIF" ? "image" : choice,
        model: "gpt-5.6-sol",
        effort: "high",
        worker_defaults: {
          model: "gpt-6-astra",
          effort: "low",
          fast_mode: false,
        },
      });
      await expect(
        page.locator(".conversation-meta .workspace-mode-badge"),
      ).toHaveText(badge);
    } finally {
      await server.close();
    }
  });
}
for (const system of ["Linux", "Windows"]) {
  test(`${system} New chat uses only worktree`, async ({ page }) => {
    const { server, creations } = await setup(system);
    try {
      const dialog = await openNewChat(page, server);
      await expect(dialog.getByText("Workspace", { exact: true })).toHaveCount(
        0,
      );
      await dialog
        .getByRole("button", { name: "Start chat", exact: true })
        .click();
      await expect.poll(() => creations.length).toBe(1);
      expect(creations[0].workspaceMode).toBe("worktree");
    } finally {
      await server.close();
    }
  });
}
for (const variant of ["light", "dark", "mobile"]) {
  test(`New chat workspace dialog ${variant}`, async ({ page }, testInfo) => {
    const { server, creations } = await setup();
    try {
      await page.setViewportSize(
        variant === "mobile"
          ? { width: 390, height: 844 }
          : { width: 1280, height: 800 },
      );
      await page.emulateMedia({
        colorScheme: variant === "dark" ? "dark" : "light",
      });
      await page.goto(server.origin + "/?studio-navigation=combined");
      if (variant === "mobile") await page.locator("#sidebar-toggle").click();
      await page
        .locator('[data-project-path="/same/project"] .project-tree-heading')
        .hover();
      await page
        .getByRole("button", { name: "New chat in Local project", exact: true })
        .click();
      const dialog = page.getByRole("dialog", {
        name: "New chat",
        exact: true,
      });
      await expect(
        dialog.getByRole("button", { name: "Start chat", exact: true }),
      ).toBeEnabled();
      await expect(
        dialog.getByRole("radio", { name: "layr", exact: true }),
      ).toBeChecked();
      await expect(
        dialog.getByRole("button", {
          name: "Main agent settings",
          exact: true,
        }),
      ).toBeVisible();
      await expect(
        dialog.getByRole("button", { name: "Subagent defaults", exact: true }),
      ).toBeVisible();
      const bounds = await dialog.boundingBox();
      const viewport = page.viewportSize();
      expect(bounds.x + bounds.width).toBeLessThanOrEqual(viewport.width);
      await page.screenshot({
        animations: "disabled",
        path: testInfo.outputPath(`new-chat-${variant}.png`),
      });
      await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
      expect(creations).toHaveLength(0);
    } finally {
      await server.close();
    }
  });
}

test("the general New chat button opens the controls for one folder", async ({
  page,
}) => {
  const { server, creations } = await setup();
  try {
    await page.goto(server.origin);
    await page
      .locator("#sidebar")
      .getByRole("button", { name: "New chat", exact: true })
      .click();
    const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
    await expect(
      dialog.getByRole("button", { name: "Start chat", exact: true }),
    ).toBeEnabled();
    await expect(
      dialog.getByRole("radio", { name: "layr", exact: true }),
    ).toBeChecked();
    expect(creations).toHaveLength(0);
  } finally {
    await server.close();
  }
});

test("a paired server uses its own platform and catalog", async ({
  page,
  context,
}) => {
  const local = await setup();
  const remote = await setup("Linux", "Remote", true);
  try {
    const project = local.server.snapshot.runtime.projects[0];
    project.homeServerId = "local";
    project.locations = [
      { serverId: "local", projectId: project.id, path: project.path },
      { serverId: "remote", projectId: project.id, path: project.path },
    ];
    local.server.discoverPeer(remote.server, "paired", "reachable");
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
      {
        destination: remote.server.origin,
        source: remote.server.invitation.origin,
      },
    );
    await page.goto(local.server.origin + "/?studio-navigation=combined");
    const group = page
      .locator('.server-sidebar [data-project-path="/same/project"]')
      .filter({
        has: page.getByRole("button", {
          name: "New chat in Local project",
          exact: true,
        }),
      });
    await group
      .getByRole("button", { name: "New chat in Local project", exact: true })
      .click();
    const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
    await dialog.getByRole("button", { name: /Remote.*Active/ }).click();
    await expect(
      dialog.getByRole("button", { name: "Start chat", exact: true }),
    ).toBeEnabled();
    await expect(dialog.getByText("Workspace", { exact: true })).toHaveCount(0);
    await expect(
      dialog.getByRole("button", { name: "Main agent settings", exact: true }),
    ).toContainText("Sol");
    await dialog
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    await expect.poll(() => remote.creations.length).toBe(1);
    expect(local.creations).toHaveLength(0);
    expect(remote.creations[0]).toMatchObject({
      workspaceMode: "worktree",
      model: "gpt-5.6-sol",
    });
    expect(remote.modelReads.length).toBeGreaterThan(0);
  } finally {
    await local.server.close();
    await remote.server.close();
  }
});
