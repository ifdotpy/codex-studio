import {
  handleEntitySyncFixtureRequest,
  syncIdentityFixture,
  test,
} from "../playwright.mjs";
// Production React build with isolated account fixtures. No credentials or model calls.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, extname } from "node:path";
import { fileURLToPath } from "node:url";

const browserContextsByTest = new WeakMap();
test.beforeEach(async ({ browser }, testInfo) => {
  browserContextsByTest.set(testInfo, new Set(browser.contexts()));
});
test.afterEach(async ({ browser }, testInfo) => {
  const initialContexts = browserContextsByTest.get(testInfo) ?? new Set();
  await Promise.all(
    browser
      .contexts()
      .filter((context) => !initialContexts.has(context))
      .map((context) => context.close()),
  );
});

test("project settings rows and requests", async ({ browser: _browser }) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const webDist =
    process.env.STUDIO_WEB_DIST ||
    join(root, "workspaces/client/apps/web/dist");
  const evidence = await mkdtemp(join(tmpdir(), "codex-project-account-ui-"));
  const accounts = [
    {
      id: "default",
      email: "personal@example.com",
      label: "Personal",
      plan: "pro",
      source: "Codex",
      status: "ready",
    },
    {
      id: "work",
      email: "work@example.com",
      label: "Work",
      plan: "plus",
      source: "CodexBar",
      status: "ready",
    },
    {
      id: "other",
      email: "another.long.account@example.com",
      label: "Second workspace",
      plan: "pro",
      source: "Profile",
      status: "ready",
    },
  ];
  accounts.push(
    ...Array.from({ length: 4 }, (_, index) => ({
      id: `extra-${index}`,
      label: `Workspace ${index + 1}`,
      email: `workspace${index + 1}@example.com`,
      provider: "codex",
      status: "ready",
    })),
  );
  const projects = [
    {
      id: "/tmp/fixture",
      path: "/tmp/fixture",
      name: "fixture",
      created: 1,
      accountKey: "work",
      accountKeys: accounts.map((account) => account.id),
      accountRevision: 1,
      workerBaseRevision: 1,
      workerEnvironmentRevision: 1,
    },
    {
      id: "/projects/Lumina",
      path: "/projects/Lumina",
      name: "Lumina",
      created: 1,
      accountKey: "other",
      accountRevision: 1,
      workerBaseRevision: 1,
      folders: [{ id: "review", name: "Review", parentId: null }],
    },
  ];
  const archivedAccounts = [];
  let conflict = false;
  const defaultAccountKey = "default";
  const makeLead = (id, name, accountKey, empty) => ({
    id,
    rootId: id,
    name,
    accountKey,
    empty,
    threadId: empty ? null : `thread-${id}`,
    isLead: true,
    source: "managed",
    status: "idle",
    model: "gpt-6-astra",
    created: Date.now() / 1000,
    inFlight: false,
    canSend: true,
    cwd: "/tmp/fixture",
  });
  const agents = [
    makeLead("started", "Started conversation", "default", false),
    makeLead("empty", "New conversation", "default", true),
  ];
  const bodies = [];
  const syncWorkspaceId = syncIdentityFixture().workspaceId;
  const stateForEntities = {
    stateDir: evidence,
    threads: agents,
    chats: [],
    runtime: {
      agents,
      projects,
      rooms: [],
      complaints: [],
      requests: [],
      monitors: [],
      tasks: [],
      work: [],
      userTasks: [],
      rateLimitsByAccount: {},
    },
  };
  const server = createServer(async (req, res) => {
    const url = new URL(req.url, "http://localhost");
    let body = {};
    if (req.method === "POST") {
      let text = "";
      for await (const chunk of req) text += chunk;
      body = JSON.parse(text || "{}");
      bodies.push({ path: url.pathname, body });
    }
    const json = (data) => {
      res.setHeader("Content-Type", "application/json");
      res.end(JSON.stringify(data));
    };
    if (
      handleEntitySyncFixtureRequest(req, res, {
        snapshot: stateForEntities,
        workspaceId: syncWorkspaceId,
        onStreamReady: (notify) =>
          notify(JSON.parse(url.searchParams.get("resources") || "[]")),
      })
    )
      return;
    if (url.pathname === "/api/session") return json({ token: "fixture" });

    if (
      url.pathname === "/api/accounts" ||
      url.pathname === "/api/accounts/discover" ||
      url.pathname === "/api/accounts/default"
    ) {
      return json({
        accounts,
        archivedAccounts,
        defaultAccountKey,
        supportsDelete: true,
        supportsDisconnect: true,
      });
    }
    if (url.pathname === "/api/projects") {
      let project = projects.find((item) => item.path === body.path);
      if (conflict) {
        res.statusCode = 409;
        return json({
          error: "Project account changed. Reopen to load the current account.",
        });
      }
      if (!project) {
        project = {
          id: body.path,
          path: body.path,
          name: body.path.split("/").at(-1),
          created: 1,
          accountRevision: 0,
          workerBaseRevision: 0,
        };
        projects.push(project);
      }
      if (body.action === "set_worker_base") {
        assert.equal(body.expected_revision, project.workerBaseRevision);
        project.workerBaseRef = body.base_ref;
        project.workerBaseRevision++;
        return json(project);
      }
      if (body.action === "set_worker_environment") {
        assert.equal(body.expected_revision, project.workerEnvironmentRevision);
        project.workerEnvironment = body.environment;
        project.workerEnvironmentRevision++;
        return json(project);
      }
      assert.equal(body.expected_revision, project.accountRevision);
      project.accountKey = body.account_key;
      project.accountKeys = body.account_keys;
      project.accountRevision++;
      return json(project);
    }
    if (url.pathname === "/api/transcript/stream") {
      res.writeHead(503);
      return res.end();
    }
    if (url.pathname === "/api/limits") {
      const accountKey = url.searchParams.get("account_key") || "default";
      const usedPercent =
        10 +
        Math.max(
          0,
          accounts.findIndex((account) => account.id === accountKey),
        ) *
          12;
      return json({
        accountKey,
        at: Date.now() / 1000,
        data: {
          rateLimits: {
            limitId: "codex",
            planType: "pro",
            primary: {
              usedPercent,
              windowDurationMins: 300,
              resetsAt: Date.now() / 1000 + 3600,
            },
          },
        },
      });
    }
    if (url.pathname.startsWith("/api/"))
      return json({ items: [], sessions: [] });
    try {
      const path = join(
        webDist,
        url.pathname === "/" ? "index.html" : url.pathname,
      );
      const file = await readFile(path);
      res.setHeader(
        "Content-Type",
        { ".js": "text/javascript", ".css": "text/css", ".html": "text/html" }[
          extname(path)
        ] || "application/octet-stream",
      );
      res.end(file);
    } catch {
      res.writeHead(404);
      res.end();
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    const page = await _browser.newPage({
      viewport: { width: 1440, height: 900 },
      colorScheme: "dark",
    });
    page.setDefaultTimeout(10000);
    const accountResponse = page.waitForResponse((response) =>
      response.url().endsWith("/api/accounts"),
    );
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await accountResponse;
    await page
      .getByText("personal@example.com", { exact: true })
      .first()
      .waitFor();
    await page.locator("[data-chat]").first().waitFor();
    const openProject = async () => {
      const button = page.getByRole("button", {
        name: "Options for project fixture",
        exact: true,
      });
      await button.locator("..").hover();
      await button.click();
      await page
        .getByRole("menuitem", { name: "Project account", exact: true })
        .click();
    };
    const dialog = page.getByRole("dialog", {
      name: "Project settings",
      exact: true,
    });
    const defaults = dialog.getByRole("group", {
      name: "Default account for new chats",
      exact: true,
    });
    const memberships = dialog.getByRole("group", {
      name: "Accounts shown first for this project",
      exact: true,
    });
    await openProject();
    await defaults.locator("[data-account-key]").first().waitFor();
    assert.equal(await defaults.locator("[data-account-key]").count(), 7);
    assert.equal(await memberships.locator("[data-account-key]").count(), 7);
    assert.equal(
      await dialog
        .getByRole("button", { name: "Save accounts", exact: true })
        .count(),
      0,
    );
    assert.equal(
      await dialog
        .getByLabel("Default worker base ref")
        .getAttribute("placeholder"),
      "main",
    );
    for (const [profile, width, colorScheme] of [
      ["dark", 1440, "dark"],
      ["light", 1440, "light"],
      ["mobile", 390, "dark"],
    ]) {
      await page.setViewportSize({ width, height: width === 390 ? 844 : 900 });
      await page.emulateMedia({ colorScheme });
      await dialog.evaluate(
        () =>
          new Promise((resolve) =>
            requestAnimationFrame(() => requestAnimationFrame(resolve)),
          ),
      );
      await page.screenshot({
        path: join(evidence, `${profile}-project-settings.png`),
        animations: "disabled",
      });
      if (width === 390) {
        await dialog
          .getByLabel("Default worker environment")
          .scrollIntoViewIfNeeded();
        await page.screenshot({
          path: join(evidence, "mobile-project-settings-bottom.png"),
          animations: "disabled",
        });
        await defaults.scrollIntoViewIfNeeded();
      }
      const bounds = await dialog.boundingBox();
      assert.ok(bounds.x >= 0 && bounds.x + bounds.width <= width);
      assert.ok(
        await dialog.evaluate(
          (element) => element.scrollWidth <= element.clientWidth,
        ),
      );
    }
    await defaults.locator('[data-account-key="other"]').click();
    conflict = true;
    const accountSave = dialog.getByRole("button", {
      name: "Save accounts",
      exact: true,
    });
    await accountSave.click();
    await dialog.getByRole("alert").waitFor();
    assert.equal(await defaults.getAttribute("data-value"), "other");
    conflict = false;
    await accountSave.click();
    await dialog.waitFor({ state: "hidden" });
    const requests = bodies.filter((item) => item.path === "/api/projects");
    assert.deepEqual(requests[0].body, requests[1].body);
    assert.equal(projects[0].accountKey, "other");
    assert.deepEqual(
      projects[0].accountKeys,
      accounts.map((account) => account.id),
    );
    await page.setViewportSize({ width: 1440, height: 900 });
    await openProject();
    await memberships.locator('[data-account-key="other"]').click();
    assert.equal(await defaults.getAttribute("data-value"), "");
    assert.equal(await accountSave.isDisabled(), true);
    await defaults.locator('[data-account-key="work"]').click();
    await accountSave.click();
    await dialog.waitFor({ state: "hidden" });
    assert.equal(projects[0].accountKey, "work");
    assert.ok(!projects[0].accountKeys.includes("other"));
    await openProject();
    await dialog.getByLabel("Default worker base ref").fill(" origin/main ");
    await dialog
      .getByRole("button", { name: "Save worker base", exact: true })
      .click();
    await dialog.waitFor({ state: "hidden" });
    assert.deepEqual(
      bodies.filter((item) => item.path === "/api/projects").at(-1).body,
      {
        action: "set_worker_base",
        path: "/tmp/fixture",
        base_ref: "origin/main",
        expected_revision: 1,
      },
    );
    await openProject();
    await dialog.getByLabel("Default worker environment").selectOption("linux");
    await dialog
      .getByRole("button", { name: "Save worker environment", exact: true })
      .click();
    await dialog.waitFor({ state: "hidden" });
    assert.deepEqual(
      bodies.filter((item) => item.path === "/api/projects").at(-1).body,
      {
        action: "set_worker_environment",
        path: "/tmp/fixture",
        environment: "linux",
        expected_revision: 1,
      },
    );
    const removed = accounts.splice(2, 1)[0];
    archivedAccounts.push({ ...removed, deleted: true });
    projects[0].accountKey = removed.id;
    projects[0].accountKeys = ["work", removed.id];
    await page.reload();
    await page.locator("[data-chat]").first().waitFor();
    await openProject();
    const deletedMembership = memberships.locator(
      `[data-account-key="${removed.id}"]`,
    );
    await deletedMembership.waitFor();
    assert.equal(await deletedMembership.getAttribute("aria-pressed"), "true");
    assert.equal(await defaults.getAttribute("data-value"), "");
    await deletedMembership.click();
    await deletedMembership.waitFor({ state: "detached" });
    assert.equal(await accountSave.isDisabled(), true);
    await defaults.locator('[data-account-key="work"]').click();
    await accountSave.click();
    await dialog.waitFor({ state: "hidden" });
    assert.equal(projects[0].accountKey, "work");
    assert.deepEqual(projects[0].accountKeys, ["work"]);
    console.log(
      JSON.stringify({
        evidence,
        accounts: 7,
        cases: [
          "three requested rows",
          "responsive dialog",
          "account revision retry",
          "default membership removal",
          "worker base request",
        ],
      }),
    );
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});
