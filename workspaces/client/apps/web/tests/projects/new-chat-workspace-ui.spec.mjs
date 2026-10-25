import { mkdir } from "node:fs/promises";
import { join } from "node:path";
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
async function setup(
  system = "Darwin",
  label = "Local",
  signed = false,
  provider = "codex",
  hasStudioVm = true,
) {
  const creations = [];
  const changes = [];
  const modelReads = [];
  const claudeModels = [
    {
      ...models[0],
      model: "claude-opus-5-5",
      displayName: "Claude Opus 5.5",
      provider: "claude",
    },
  ];
  const server = await fixture(label, signed, {
    ...(provider === "claude"
      ? {
          accounts: [
            {
              id: "default",
              label: "Claude",
              provider: "claude",
              status: "ready",
              isDefault: true,
            },
            { id: "codex", label: "Codex", provider: "codex", status: "ready" },
          ],
        }
      : {}),
    handle({ url, body, request, json, snapshot }) {
      if (url.pathname === "/api/projects") {
        json({ items: snapshot.runtime.projects });
        return true;
      }
      if (url.pathname === "/api/models") {
        modelReads.push(Object.fromEntries(url.searchParams));
        json({
          data:
            label === "Remote"
              ? [models[2]]
              : provider === "claude"
                ? url.searchParams.has("workers")
                  ? [...claudeModels, ...models]
                  : url.searchParams.get("account_key") === "codex"
                    ? models
                    : claudeModels
                : models,
        });
        return true;
      }
      if (url.pathname === "/api/ui-summary") {
        json({
          ready: true,
          busy: false,
          system,
          hasStudioVm,
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
  ["Layr", "Layr"],
  ["Layr VM", "Layr VM"],
  ["WT", "WT"],
]) {
  test(`macOS New chat ${choice} and actual model menus`, async ({ page }) => {
    const { server, creations, changes } = await setup();
    try {
      const dialog = await openNewChat(page, server);
      await expect(
        dialog.getByRole("radio", { name: "Layr", exact: true }),
      ).toBeChecked();
      await dialog.getByText(choice, { exact: true }).click();
      const main = dialog.getByRole("region", {
        name: "Lead model",
        exact: true,
      });
      await main.getByRole("option", { name: /Sol/ }).click();
      await main.getByText("High", { exact: true }).click();
      const worker = dialog.getByRole("region", {
        name: "Worker model",
        exact: true,
      });
      await worker.getByRole("option", { name: /Astra/ }).click();
      await worker.getByText("Low", { exact: true }).click();
      await expect(page.getByRole("dialog")).toHaveCount(1);
      expect(changes).toHaveLength(0);
      await dialog
        .getByRole("button", { name: "Start chat", exact: true })
        .click();
      await expect.poll(() => creations.length).toBe(1);
      expect(creations[0]).toMatchObject({
        workspaceMode:
          choice === "Layr"
            ? "layr-native"
            : choice === "Layr VM"
              ? "layr"
              : "worktree",
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
for (const system of ["Windows"]) {
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
for (const width of [320, 390, 1440])
  for (const variant of ["light", "dark"]) {
    test(`New chat workspace dialog ${width} ${variant}`, async ({
      page,
    }, testInfo) => {
      const { server, creations } = await setup();
      try {
        await page.setViewportSize({ width, height: 900 });
        await page.emulateMedia({
          colorScheme: variant === "dark" ? "dark" : "light",
        });
        await page.goto(server.origin + "/?studio-navigation=combined");
        await page
          .locator('[data-project-path="/same/project"] .project-tree-heading')
          .hover();
        await page
          .getByRole("button", {
            name: "New chat in Local project",
            exact: true,
          })
          .click();
        const dialog = page.getByRole("dialog", {
          name: "New chat",
          exact: true,
        });
        await expect(
          dialog.getByRole("button", { name: "Start chat", exact: true }),
        ).toBeEnabled();
        await expect(
          dialog.getByRole("radio", { name: "Layr", exact: true }),
        ).toBeChecked();
        await expect(
          dialog.getByRole("listbox", {
            name: "Main agent model",
            exact: true,
          }),
        ).toBeVisible();
        await expect(
          dialog.getByRole("listbox", {
            name: "Default subagent model",
            exact: true,
          }),
        ).toBeVisible();
        const bounds = await dialog.boundingBox();
        const viewport = page.viewportSize();
        expect(bounds.x + bounds.width).toBeLessThanOrEqual(viewport.width);
        const evidence = process.env.MODELS_EVIDENCE;
        if (evidence) await mkdir(evidence, { recursive: true });
        await page.screenshot({
          animations: "disabled",
          path: evidence
            ? join(evidence, `new-chat-${width}-${variant}.png`)
            : testInfo.outputPath(`new-chat-${width}-${variant}.png`),
        });
        await dialog
          .getByRole("button", { name: "Cancel", exact: true })
          .click();
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
      dialog.getByRole("radio", { name: "Layr", exact: true }),
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
    // A local exclusion must not hide the remote server's only model.
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const settings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await settings.getByRole("tab", { name: "Models", exact: true }).click();
    await expect(
      settings.getByText("Models on This computer", { exact: true }),
    ).toBeVisible();
    await settings
      .getByRole("checkbox", { name: "Sol 5.6", exact: true })
      .uncheck();
    await page.keyboard.press("Escape");
    // With two sources, the combined sidebar keys the logical project by its group.
    const group = page.locator("#sidebar .sidebar-project").filter({
      has: page.getByRole("button", {
        name: "New chat in Local project",
        exact: true,
      }),
    });
    await group.locator(".project-tree-heading").hover();
    await group
      .getByRole("button", { name: "New chat in Local project", exact: true })
      .click();
    const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
    await dialog.getByRole("button", { name: /Remote.*Active/ }).click();
    await expect(
      dialog.getByRole("button", { name: "Start chat", exact: true }),
    ).toBeEnabled();
    await expect(
      dialog.getByRole("radio", { name: "Layr", exact: true }),
    ).toBeChecked();
    await expect(
      dialog
        .getByRole("listbox", { name: "Main agent model", exact: true })
        .getByRole("option", { selected: true }),
    ).toHaveAccessibleName(/Sol/);
    await dialog
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    await expect.poll(() => remote.creations.length).toBe(1);
    expect(local.creations).toHaveLength(0);
    expect(remote.creations[0]).toMatchObject({
      workspaceMode: "layr-native",
      model: "gpt-5.6-sol",
    });
    expect(remote.modelReads.length).toBeGreaterThan(0);
  } finally {
    await local.server.close();
    await remote.server.close();
  }
});

test("a filtered Claude lead keeps the server's Codex worker default without injecting settings", async ({
  page,
}) => {
  const { server, creations } = await setup("Darwin", "Local", false, "claude");
  try {
    const dialog = await openNewChat(page, server);
    await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const settings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await settings.getByRole("tab", { name: "Models", exact: true }).click();
    await settings
      .getByRole("checkbox", { name: "Sol 5.6", exact: true })
      .uncheck();
    await page.keyboard.press("Escape");
    await page
      .locator("#sidebar .projects-heading")
      .getByRole("button", { name: "New chat", exact: true })
      .click();
    await expect(
      dialog
        .getByRole("listbox", { name: "Main agent model", exact: true })
        .getByRole("option", { selected: true }),
    ).toHaveAccessibleName(/Opus 5.5/);
    await expect(
      dialog
        .getByRole("listbox", { name: "Default subagent model", exact: true })
        .getByRole("option", { selected: true }),
    ).toHaveAccessibleName(/Luna 6/);
    await dialog
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    await expect.poll(() => creations.length).toBe(1);
    expect(creations[0].model).toBe("claude-opus-5-5");
    expect(creations[0]).not.toHaveProperty("worker_defaults");
  } finally {
    await server.close();
  }
});

for (const system of ["Darwin", "Linux"]) {
  test(`${system} defaults to native Layr and hides the absent Studio VM`, async ({
    page,
  }) => {
    const { server, creations } = await setup(
      system,
      "Local",
      false,
      "codex",
      false,
    );
    try {
      const dialog = await openNewChat(page, server);
      await expect(
        dialog.getByRole("radio", { name: "Layr", exact: true }),
      ).toBeChecked();
      await expect(
        dialog.getByRole("radio", { name: "Layr VM", exact: true }),
      ).toHaveCount(0);
      await expect(
        dialog.getByRole("radio", { name: "WT", exact: true }),
      ).toBeVisible();
      await dialog
        .getByRole("button", { name: "Start chat", exact: true })
        .click();
      await expect.poll(() => creations.length).toBe(1);
      expect(creations[0].workspaceMode).toBe("layr-native");
    } finally {
      await server.close();
    }
  });
}

test("Linux offers Layr VM only when its server has the Studio VM", async ({
  page,
}) => {
  const { server, creations } = await setup("Linux");
  try {
    const dialog = await openNewChat(page, server);
    await expect(
      dialog.getByRole("radio", { name: "Layr", exact: true }),
    ).toBeChecked();
    await dialog.getByRole("radio", { name: "Layr VM", exact: true }).check();
    await dialog
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    await expect.poll(() => creations.length).toBe(1);
    expect(creations[0].workspaceMode).toBe("layr");
  } finally {
    await server.close();
  }
});
