import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect, createFixtureViteServer } from "../playwright.mjs";
import { fixture } from "./multi-server-fixture.mjs";

test("team, task assignments and projects render typed stream changes without extra state reads", async ({
  page,
}) => {
  const scratch = await mkdtemp(join(tmpdir(), "studio-team-work-stream-"));
  const task = {
    id: "assignment",
    rootId: "overlap",
    title: "Inspect stream delivery",
    description: "Preserve gate evidence and decisions",
    status: "running",
    owner: "worker-first",
    dependencies: [],
    version: 1,
    created: 1,
    updated: 1,
    results: [],
    decisions: [],
    gate: null,
  };
  const reads = [];
  const projectWrites = [];
  let projectReadBaseline = 0;
  const errors = [];
  const backend = await fixture("Team", false, {
    entityFrames: true,
    handle({ url, request, body, json, snapshot }) {
      if (url.pathname === "/api/directories") {
        json({
          path: url.searchParams.get("path") || "/fixture",
          parent: "/",
          directories: [],
        });
        return true;
      }
      if (url.pathname === "/api/projects" && request.method === "POST") {
        projectWrites.push(body);
        const project = {
          id: body.path,
          path: body.path,
          name: "Created live project",
          folders: [],
          peerTeams: [],
        };
        snapshot.runtime.projects.push(project);
        json(project);
        backend.publishState();
        return true;
      }
      if (url.pathname !== "/api/work" || request.method !== "GET")
        return false;
      json({
        rootId: "overlap",
        taskDefaultGate: null,
        items: [task],
        tasks: [task],
      });
      return true;
    },
  });
  const lead = backend.snapshot.runtime.agents[0];
  backend.snapshot.runtime.viewScopedAgents = true;
  const worker = {
    ...lead,
    id: "worker-first",
    name: "First stream worker",
    isLead: false,
    parentId: lead.id,
    rootId: lead.id,
    status: "running",
    inFlight: true,
    commandProgress: [
      {
        monitorId: "check",
        stage: "Inspecting stream",
        startedAt: 1,
        lastOutputLine: "Initial progress",
      },
    ],
  };
  backend.snapshot.runtime.agents.push(worker);
  backend.snapshot.runtime.work = [
    { id: task.id, rootId: lead.id, title: task.title, status: task.status },
  ];
  backend.snapshot.runtime.workDetails = [task];
  backend.snapshot.runtime.agentDetails = backend.snapshot.runtime.agents;
  const vite = await createFixtureViteServer({
    configFile: false,
    root: fileURLToPath(new URL("../..", import.meta.url)),
    server: { host: "127.0.0.1", port: 0, proxy: { "/api": backend.origin } },
  });
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (request.method() === "POST" && url.pathname === "/api/projects")
      projectReadBaseline = reads.length;
    if (
      request.method() === "GET" &&
      url.pathname.startsWith("/api/") &&
      url.pathname !== "/api/sync/stream"
    )
      reads.push(url.pathname + url.search);
  });
  try {
    await vite.listen();
    await page.setViewportSize({ width: 1440, height: 960 });
    await page.goto(`http://127.0.0.1:${vite.httpServer.address().port}`);
    await expect(page.locator("#message")).toBeVisible({ timeout: 20_000 });
    await page.locator("#team-toggle").click();
    const team = page.locator("#team");
    await expect(team.locator('[data-team-count="all"] strong')).toHaveText(
      "1",
    );
    await expect(
      team.getByText("First stream worker", { exact: true }),
    ).toBeVisible();
    await expect(team.locator('[data-worker="worker-first"]')).toContainText(
      "Inspecting stream",
    );
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator('[data-workspace-section="plan"]').click();
    await page
      .getByRole("navigation", { name: "Workspace sections" })
      .getByRole("button", { name: "Tasks", exact: true })
      .click();
    const tasks = page.getByRole("region", { name: "Tasks", exact: true });
    await expect(
      tasks.getByRole("button", { name: /Inspect stream delivery/ }),
    ).toContainText("running");
    const baselineReads = reads.length;
    const newWorker = {
      ...worker,
      id: "worker-second",
      name: "Second stream worker",
      commandProgress: [],
    };
    backend.snapshot.runtime.agents.push(newWorker);
    worker.status = "failed";
    worker.inFlight = false;
    worker.error = "Stream regression failure";
    worker.commandProgress = [];
    Object.assign(task, { status: "review", version: 2, updated: 2 });
    task.gate = { command: "verify", timeout_ms: 1234 };
    task.results = [
      {
        id: "stream-result",
        agent: worker.id,
        text: "Full streamed review evidence",
        checks: "Checked the stream",
        revision: "abc123",
        files: [],
        created: 2,
        gate: {
          status: "failed",
          config: task.gate,
          exitCode: 1,
          output: "Full streamed gate output",
          revision: "abc123",
          monitorId: "gate-check",
        },
      },
    ];
    task.decisions = [
      {
        decision: "reject",
        reason: "Streamed review decision",
        by: lead.id,
        created: 2,
      },
    ];
    backend.snapshot.runtime.work[0].status = task.status;
    backend.publishState([
      { kind: "state" },
      { kind: "work", agentId: lead.id },
      { kind: "agentDetail", agentId: worker.id },
    ]);
    await expect(
      tasks.getByRole("button", { name: /Inspect stream delivery/ }),
    ).toContainText("review");
    await tasks
      .getByRole("button", { name: /Inspect stream delivery/ })
      .click();
    const detail = tasks.getByRole("article", { name: "Task detail" });
    await expect(detail.getByLabel("Gate output")).toHaveText(
      "Full streamed gate output",
    );
    await detail.getByText("Result history (1)", { exact: true }).click();
    await expect(detail.getByLabel("Result stream-result")).toContainText(
      "Full streamed review evidence",
    );
    await detail.getByText("Decision history (1)", { exact: true }).click();
    await expect(detail.getByLabel("Decision 1")).toContainText(
      "Streamed review decision",
    );
    await page.locator(".workspace-drawer .mantine-Drawer-close").click();
    await expect(team.locator('[data-team-count="all"] strong')).toHaveText(
      "2",
    );
    await expect(
      team.getByText("Second stream worker", { exact: true }),
    ).toBeVisible();
    await expect(team.locator('[data-worker="worker-first"]')).toContainText(
      "Stream regression failure",
    );
    newWorker.commandProgress = [
      {
        monitorId: "second-check",
        stage: "Updated streamed progress",
        startedAt: 2,
        lastOutputLine: "Completed the next check",
      },
    ];
    backend.publishState([{ kind: "agentDetail", agentId: newWorker.id }]);
    await expect(team.locator('[data-worker="worker-second"]')).toContainText(
      "Updated streamed progress",
    );
    newWorker.status = "interrupted";
    newWorker.inFlight = false;
    newWorker.commandProgress = [];
    backend.publishState([
      { kind: "state" },
      { kind: "agentDetail", agentId: newWorker.id },
    ]);
    await expect(team.locator('[data-worker="worker-second"]')).toContainText(
      "Interrupted",
    );
    backend.snapshot.runtime.projects.push({
      id: "/new/live/project",
      path: "/new/live/project",
      name: "New live project",
      folders: [],
      peerTeams: [],
    });
    backend.publishState();
    await expect(
      page.getByRole("button", { name: "New live project", exact: true }),
    ).toBeVisible();
    backend.snapshot.runtime.agents.splice(
      backend.snapshot.runtime.agents.indexOf(newWorker),
      1,
    );
    backend.publishState();
    await expect(team.locator('[data-team-count="all"] strong')).toHaveText(
      "1",
    );
    await expect(
      team.getByText("Second stream worker", { exact: true }),
    ).toHaveCount(0);
    const inactive = {
      ...lead,
      id: "inactive",
      rootId: "inactive",
      name: "Original inactive title",
    };
    backend.snapshot.runtime.agents.push(inactive);
    backend.publishState();
    await expect(page.locator('[data-chat="inactive"]')).toContainText(
      "Original inactive title",
    );
    lead.name = "Updated active title";
    inactive.name = "Updated inactive title";
    backend.publishState();
    await expect(page.locator(".workspace-header")).toContainText(
      "Updated active title",
    );
    await expect(page.locator('[data-chat="overlap"]')).toContainText(
      "Updated active title",
    );
    await expect(page.locator('[data-chat="inactive"]')).toContainText(
      "Updated inactive title",
    );
    expect(reads.slice(baselineReads)).toEqual([]);
    await page
      .getByRole("button", { name: "Add project", exact: true })
      .click();
    const picker = page.getByRole("dialog", {
      name: "Add project",
      exact: true,
    });
    await expect(picker.locator(".directory-picker")).toHaveAttribute(
      "aria-busy",
      "false",
    );
    await picker.getByLabel("Folder path").fill("/created/from/ui");
    await picker
      .getByRole("button", { name: "Open path", exact: true })
      .click();
    await expect(picker.locator(".directory-location")).toHaveText(
      "/created/from/ui",
    );
    await picker
      .getByRole("button", { name: "Add project", exact: true })
      .click();
    await expect(picker).toBeHidden();
    await expect(
      page.getByRole("button", { name: "Created live project", exact: true }),
    ).toBeVisible();
    expect(projectWrites).toEqual([{ path: "/created/from/ui" }]);
    expect(reads.slice(projectReadBaseline)).toEqual([]);
    expect(errors).toEqual([]);
  } finally {
    await page.close();
    await vite.close();
    await backend.close();
    await rm(scratch, { recursive: true, force: true });
  }
});
