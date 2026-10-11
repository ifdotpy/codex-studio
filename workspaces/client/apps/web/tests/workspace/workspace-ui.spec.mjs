import {
  apiSchemaHeaders,
  readTestState,
  spawnFixture as spawn,
  test,
} from "../playwright.mjs";
// Exercise workspace actions through the real runtime and HTTP server.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

test("workspace ui", async ({ page: testPage }) => {
  test.setTimeout(120_000);
  const skill = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const root = await mkdtemp(join(tmpdir(), "codex-workspace-ui-"));
  await writeFile(
    join(root, ".gitignore"),
    "*\n!.gitignore\n!report.md\n!preview.html\n",
  );
  await writeFile(
    join(root, "report.md"),
    "# Release evidence\nOriginal report.\n",
  );
  await writeFile(
    join(root, "preview.html"),
    '<h1>Report preview</h1><script>parent.document.body.dataset.unsafe="yes"</script>',
  );
  for (const args of [
    ["init", "-q"],
    ["config", "user.email", "fixture@localhost"],
    ["config", "user.name", "Fixture"],
    ["add", "."],
    ["commit", "-qm", "Fixture base"],
  ])
    execFileSync("git", args, { cwd: root });
  await writeFile(
    join(root, "report.md"),
    "# Release evidence\nVerified report for orchestration.\n++ b/other-chat.txt\n",
  );
  const proc = spawn(
    "python3",
    [
      "-B",
      join(skill, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    {
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  let log = "",
    page;
  proc.stderr.on("data", (d) => (log += d));
  const poll = async (fn, label) => {
    for (let i = 0; i < 180; i++) {
      if (await fn()) return;
      await new Promise((r) => setTimeout(r, 50));
    }
    throw Error(label + "\n" + log);
  };
  try {
    const port = await new Promise((resolve, reject) => {
      proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
      proc.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const get = async (path) => (await fetch(origin + path)).json();
    const initial = await readTestState(origin);
    const lead = initial.runtime.agents.find((a) => a.name === "Release lead");
    const post = async (path, body) => {
      const r = await fetch(origin + path, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": initial.token,
          Origin: origin,
        },
        body: JSON.stringify(body),
      });
      assert.equal(r.status, 200, await r.clone().text());
      return r.json();
    };
    const notifyWorkspace = async () => {
      const response = await fetch(new URL("/api/sync/notify", origin), {
        method: "POST",
        headers: {
          Origin: origin,
          "Content-Type": "application/json",
          "X-Canvas-Token": initial.token,
          ...apiSchemaHeaders(),
        },
        body: JSON.stringify({
          requestId: randomUUID(),
          resources: [{ kind: "workspace", agentId: lead.id }],
        }),
      });
      assert.equal(response.status, 200, await response.text());
    };
    page = testPage;
    await page.setViewportSize({ width: 1440, height: 980 });
    const errors = [];
    const resourceRequests = [];
    const annotationRequests = [];
    const planNotModified = [];
    page.on("request", (request) => {
      const url = new URL(request.url());
      if (url.pathname === "/api/resources")
        resourceRequests.push(request.url());
      if (
        url.pathname === "/api/workspace" &&
        url.searchParams.get("view") === "annotations"
      )
        annotationRequests.push(request.url());
    });
    page.on("response", (response) => {
      if (
        new URL(response.url()).pathname === "/api/plan" &&
        response.status() === 304
      )
        planNotModified.push(response);
    });
    page.on("pageerror", (e) => errors.push(e.message));
    await page.goto(origin);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator('[data-workspace-section="changes"]').click();
    const drawer = page.locator(".workspace-drawer .mantine-Drawer-content");
    const section = async (name) => {
      if (
        !(await page
          .getByRole("navigation", { name: "Workspace sections" })
          .isVisible())
      ) {
        await drawer.locator(".mantine-Drawer-close").click();
        await page
          .getByRole("button", { name: "Chat actions", exact: true })
          .click();
        await page.locator('[data-workspace-section="changes"]').click();
      }
      await page
        .getByRole("navigation", { name: "Workspace sections" })
        .getByRole("button", { name, exact: true })
        .click();
    };
    let modal;
    const initialPlanRead = page.waitForResponse((response) => {
      const url = new URL(response.url());
      return url.pathname === "/api/plan" && response.status() === 200;
    });
    await section("Plan");
    await initialPlanRead;
    await drawer.getByText("This agent has not reported a plan.").waitFor();
    const unchangedPlanRefresh = page.waitForResponse((response) => {
      const url = new URL(response.url());
      return url.pathname === "/api/plan" && response.status() === 304;
    });
    await notifyWorkspace();
    await unchangedPlanRefresh;
    await poll(
      async () => planNotModified.length > 0,
      "unchanged workspace plan uses a conditional 304 refresh",
    );
    assert.ok(planNotModified.length > 0);
    await drawer.getByText("This agent has not reported a plan.").waitFor();
    assert.equal(await drawer.getByRole("textbox").count(), 0);
    assert.equal(
      await drawer
        .getByRole("button", { name: "Save plan", exact: true })
        .count(),
      0,
    );
    const legacyPlan = await post("/api/plan", {
      agent: lead.id,
      text: "Legacy saved plan remains in storage.",
      version: 0,
    });
    await drawer.getByRole("button", { name: "Refresh workspace" }).click();
    await drawer.getByText("This agent has not reported a plan.").waitFor();
    assert.equal(await drawer.getByText(legacyPlan.text).count(), 0);
    let planAgent;
    await poll(async () => {
      planAgent = (await readTestState(origin)).runtime.agents.find(
        (agent) => agent.id === lead.id,
      );
      return !!planAgent.threadId;
    }, "plan agent has a native thread");
    proc.stdin.write(
      JSON.stringify({
        method: "turn/plan/updated",
        params: {
          threadId: planAgent.threadId,
          turnId: planAgent.turnId,
          explanation: "Inspect the evidence before release.",
          plan: [
            { step: "Inspect evidence", status: "completed" },
            { step: "Accept the release", status: "in_progress" },
          ],
        },
      }) + "\n",
    );
    await poll(
      async () =>
        (await get("/api/plan?agent=" + lead.id)).native?.plan?.length === 2,
      "agent plan persisted",
    );
    await drawer.getByRole("button", { name: "Refresh workspace" }).click();
    await drawer.getByText("Inspect evidence", { exact: true }).waitFor();
    await drawer.getByText("Accept the release", { exact: true }).waitFor();
    await drawer
      .getByText("Inspect the evidence before release.", { exact: true })
      .waitFor();
    assert.equal(await drawer.getByRole("textbox").count(), 0);
    await drawer
      .getByRole("button", { name: "Change plan in chat", exact: true })
      .click();
    await drawer.waitFor({ state: "hidden" });
    assert.equal(
      await page
        .locator(".sidebar-row.selected [data-chat]")
        .getAttribute("data-chat"),
      lead.id,
    );
    const unchangedPlan = await get("/api/plan?agent=" + lead.id);
    assert.equal(unchangedPlan.text, legacyPlan.text);
    assert.equal(unchangedPlan.version, legacyPlan.version);
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator('[data-workspace-section="changes"]').click();
    await section("Search");
    await drawer
      .getByRole("textbox", { name: "Search all conversations" })
      .fill("Review the release");
    await drawer
      .locator('form:has([aria-label="Search all conversations"])')
      .getByRole("button", { name: "Search", exact: true })
      .click();
    await drawer.getByRole("button", { name: /Review the release/ }).click();
    const searchSource = page.getByRole("dialog", { name: "Search source" });
    await searchSource
      .getByText("Review the release", { exact: false })
      .waitFor();
    await searchSource
      .getByRole("button", { name: "Close", exact: true })
      .click();
    await section("Rules");
    await drawer.getByRole("button", { name: "New rule", exact: true }).click();
    modal = page.locator(".mantine-Modal-content:visible").last();
    await modal.getByLabel("Name").fill("Release watch");
    await modal
      .getByLabel("Message to the agent")
      .fill("Check release evidence.");
    await page.reload();
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator('[data-workspace-section="changes"]').click();
    await section("Rules");
    modal = page.locator(".mantine-Modal-content:visible").last();
    assert.equal(await modal.getByLabel("Name").inputValue(), "Release watch");
    assert.equal(
      await modal.getByLabel("Message to the agent").inputValue(),
      "Check release evidence.",
    );
    await modal.getByRole("button", { name: "Save rule", exact: true }).click();
    await poll(
      async () => (await get("/api/rules")).rules.length === 1,
      "rule persisted",
    );
    await drawer.getByRole("button", { name: "Pause", exact: true }).click();
    await poll(
      async () => (await get("/api/rules")).rules[0].status === "paused",
      "rule paused",
    );
    await drawer.getByRole("button", { name: "Delete", exact: true }).click();
    await page
      .getByRole("dialog")
      .last()
      .getByRole("button", { name: "Delete rule", exact: true })
      .click();
    await poll(
      async () => (await get("/api/rules")).rules.length === 0,
      "rule deleted",
    );
    const desktopViewport = page.viewportSize();
    await page.setViewportSize({ width: 390, height: 844 });
    await page.waitForTimeout(500);
    if (!(await drawer.isVisible())) {
      await page
        .getByRole("button", { name: "Chat actions", exact: true })
        .click();
      await page.locator('[data-workspace-section="changes"]').click();
    }
    await drawer.waitFor();
    await section("Rules");
    await drawer.getByRole("button", { name: "New rule", exact: true }).click();
    modal = page.locator(".mantine-Modal-content:visible").last();
    await modal.getByLabel("Script check (optional)").fill("exit 1");
    await modal.getByLabel("Trigger").selectOption("low_workers");
    assert.equal(
      await modal.getByLabel("Minimum active subagents").inputValue(),
      "8",
    );
    assert.equal(
      await modal.getByLabel("Duration (minutes)").inputValue(),
      "30",
    );
    assert.equal(
      await modal.getByLabel("Name").inputValue(),
      "Too few active subagents",
    );
    assert.ok(
      (await modal.getByLabel("Message to the main agent").inputValue()).trim(),
    );
    assert.equal(await modal.getByLabel("Script check (optional)").count(), 0);
    await modal.getByLabel("Minimum active subagents").fill("3");
    await modal.getByLabel("Duration (minutes)").fill("2");
    await page.screenshot({
      path: join(root, "low-workers-mobile.png"),
      fullPage: true,
      animations: "disabled",
    });
    assert.ok(
      await modal.evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
      "low worker alert modal does not overflow at 390px",
    );
    await modal.getByRole("button", { name: "Save rule", exact: true }).click();
    await poll(
      async () => (await get("/api/rules")).rules.length === 1,
      "low worker alert persisted",
    );
    const lowWorkers = (await get("/api/rules")).rules[0];
    assert.equal(lowWorkers.kind, "low_workers");
    assert.equal(lowWorkers.agent, lead.id);
    assert.equal(lowWorkers.minimumWorkers, 3);
    assert.equal(lowWorkers.durationMinutes, 2);
    assert.equal(lowWorkers.command, "");
    assert.ok(lowWorkers.text.trim());
    await page.setViewportSize(desktopViewport);
    await page.waitForTimeout(500);
    if (!(await drawer.isVisible())) {
      await page
        .getByRole("button", { name: "Chat actions", exact: true })
        .click();
      await page.locator('[data-workspace-section="changes"]').click();
    }
    await drawer.waitFor();
    await section("Rules");
    await drawer
      .getByText("Fewer than 3 active subagents for more than 2 minutes", {
        exact: true,
      })
      .waitFor();
    await drawer.getByRole("button", { name: "Pause", exact: true }).click();
    await poll(
      async () => (await get("/api/rules")).rules[0].status === "paused",
      "low worker alert paused",
    );
    await drawer.getByRole("button", { name: "Resume", exact: true }).click();
    await poll(
      async () => (await get("/api/rules")).rules[0].status === "active",
      "low worker alert resumed",
    );
    await drawer.getByRole("button", { name: "Delete", exact: true }).click();
    await page
      .getByRole("dialog")
      .last()
      .getByRole("button", { name: "Delete rule", exact: true })
      .click();
    await poll(
      async () => (await get("/api/rules")).rules.length === 0,
      "low worker alert deleted",
    );
    await section("Profiles");
    await drawer
      .getByRole("button", { name: "New profile", exact: true })
      .click();
    modal = page.locator(".mantine-Modal-content:visible").last();
    await modal.getByLabel("Name").fill("Evidence reviewer");
    await modal.getByLabel("Role").selectOption("reviewer");
    await modal
      .getByLabel("Instructions")
      .fill("Read the complete diff and verify the result.");
    await page.reload();
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator('[data-workspace-section="changes"]').click();
    await section("Profiles");
    modal = page.locator(".mantine-Modal-content:visible").last();
    assert.equal(
      await modal.getByLabel("Name").inputValue(),
      "Evidence reviewer",
    );
    assert.equal(
      await modal.getByLabel("Instructions").inputValue(),
      "Read the complete diff and verify the result.",
    );
    await modal
      .getByRole("button", { name: "Save profile", exact: true })
      .click();
    await poll(
      async () => (await get("/api/profiles")).profiles.length === 1,
      "profile persisted",
    );
    const savedProfile = (await get("/api/profiles")).profiles.find(
      (profile) => profile.name === "Evidence reviewer",
    );
    assert.ok(savedProfile?.id, "saved profile has an identity");
    await drawer
      .getByRole("button", { name: "Start subagent", exact: true })
      .click();
    modal = page.locator(".mantine-Modal-content:visible").last();
    await modal
      .getByLabel("Task for this subagent")
      .fill("Review the release evidence using the saved profile.");
    const createRequest = page.waitForRequest((request) => {
      const url = new URL(request.url());
      return url.pathname === "/api/agents" && request.method() === "POST";
    });
    const createResponse = page.waitForResponse((response) => {
      const url = new URL(response.url());
      return (
        url.pathname === "/api/agents" &&
        response.request().method() === "POST" &&
        response.status() === 200
      );
    });
    await modal
      .getByRole("button", { name: "Start subagent", exact: true })
      .click();
    const request = await createRequest;
    assert.equal(
      request.postDataJSON().profile_id,
      savedProfile.id,
      "subagent creation carries the selected saved profile ID",
    );
    const created = await createResponse.then((response) => response.json());
    await poll(
      async () =>
        (await readTestState(origin)).runtime.agents.some(
          (a) =>
            a.id === created.id &&
            a.name === "Evidence reviewer" &&
            a.parentId === lead.id &&
            a.role === "reviewer",
        ),
      "profile worker created in selected team with its saved role",
    );
    assert.equal(
      await drawer
        .getByRole("button", { name: "Resources", exact: true })
        .count(),
      0,
      "resource reservations are not available in the workspace",
    );
    const changedLead = (await readTestState(origin)).runtime.agents.find(
      (agent) => agent.id === lead.id,
    );
    assert.ok(changedLead.threadId, "fixture lead has a native thread");
    proc.stdin.write(
      JSON.stringify({
        method: "turn/diff/updated",
        params: {
          threadId: changedLead.threadId,
          turnId: changedLead.turnId,
          diff: execFileSync("git", ["diff", "HEAD", "--no-color"], {
            cwd: root,
            encoding: "utf8",
          }),
        },
      }) + "\n",
    );
    await poll(
      async () =>
        (
          await get("/api/changes?agent=" + lead.id + "&scope=chat")
        ).diff?.includes("Verified report"),
      "chat diff recorded",
    );
    await section("Changes");
    await drawer.getByRole("region", { name: "Changes diff" }).waitFor();
    assert.equal(
      await drawer
        .getByRole("button", {
          name: "Comment on other-chat.txt line 3",
          exact: true,
        })
        .count(),
      0,
    );
    await drawer
      .getByRole("button", { name: "Comment on report.md line 3", exact: true })
      .waitFor();
    await drawer
      .getByRole("button", { name: "Comment on report.md line 2", exact: true })
      .click();
    modal = page.locator(".mantine-Modal-content:visible").last();
    await modal
      .getByLabel("Comment to the agent")
      .fill("Keep the revision next to these results.");
    await modal
      .getByRole("button", { name: "Send comment", exact: true })
      .click();
    await poll(
      async () =>
        (await get("/api/workspace?agent=" + lead.id + "&view=annotations"))
          .annotations.length === 1,
      "line comment stored",
    );
    assert.ok(
      annotationRequests.length > 0,
      "Changes requests only annotations",
    );
    await drawer
      .getByRole("textbox", { name: "Open a file" })
      .fill("preview.html");
    await drawer
      .getByRole("button", { name: "Preview file", exact: true })
      .click();
    await page
      .frameLocator(".workspace-preview-frame")
      .getByRole("heading", { name: "Report preview" })
      .waitFor();
    assert.equal(await page.locator("body").getAttribute("data-unsafe"), null);
    await page
      .getByRole("dialog")
      .last()
      .getByRole("button", { name: "Close", exact: true })
      .click();
    // Stop queued work before a checkpoint. The shared root remains read-only for restore.
    await post("/api/stop", { id: lead.id, descendants: true });
    await section("Checkpoints");
    await drawer
      .getByRole("textbox", { name: "Checkpoint name" })
      .fill("Evidence reviewed");
    await drawer
      .getByRole("button", { name: "Save checkpoint", exact: true })
      .click();
    await poll(
      async () =>
        (await get("/api/checkpoints?agent=" + lead.id)).checkpoints.length > 0,
      "checkpoint saved",
    );
    await drawer
      .getByRole("button", { name: "Preview restore", exact: true })
      .last()
      .click();
    await page
      .getByRole("dialog")
      .last()
      .getByText(
        "A restore requires an idle agent with an isolated worktree.",
        {
          exact: true,
        },
      )
      .waitFor();
    assert.equal(
      await page
        .getByRole("dialog")
        .last()
        .getByRole("button", { name: "Restore this checkpoint", exact: true })
        .isDisabled(),
      true,
    );
    await page
      .getByRole("dialog")
      .last()
      .getByRole("button", { name: "Close", exact: true })
      .click();
    for (const name of ["Tools", "Messages"]) {
      await section(name);
      await drawer.getByRole("heading", { name, exact: true }).waitFor();
    }
    await page.setViewportSize({ width: 390, height: 844 });
    await page.waitForTimeout(500);
    if (!(await drawer.isVisible())) {
      await page
        .getByRole("button", { name: "Chat actions", exact: true })
        .click();
      await page.locator('[data-workspace-section="changes"]').click();
    }
    await drawer.waitFor();
    await section("Changes");
    await page.screenshot({
      path: join(root, "changes-mobile.png"),
      fullPage: true,
    });
    assert.ok(
      await drawer.evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
      "drawer does not overflow at 390px",
    );
    for (const name of [
      "Changes",
      "Search",
      "Plan",
      "Checkpoints",
      "Tools",
      "Profiles",
      "Rules",
      "Messages",
    ]) {
      await section(name);
      assert.ok(
        await drawer.evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
        name + " no mobile overflow",
      );
    }
    await drawer.locator(".mantine-Drawer-close").click();
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator("#tasks-toggle").click();
    const activity = page
      .getByRole("dialog")
      .filter({ hasText: "Current activity" });
    await activity.getByRole("region", { name: "Task list" }).waitFor();
    await activity.getByText("No active tasks", { exact: true }).waitFor();
    await page.keyboard.press("Escape");
    assert.deepEqual(
      resourceRequests,
      [],
      "workspace never requests reservations",
    );
    assert.deepEqual(errors, []);
    console.log(
      "PASS workspace UI: plan, search, rule lifecycle, profile launch, line comment, safe HTML preview, checkpoint preview, 390px sections. Evidence: " +
        root,
    );
  } catch (error) {
    if (page && !page.isClosed()) {
      await page.screenshot({
        path: join(root, "failure.png"),
        fullPage: true,
      });
    }
    console.error("Evidence:", root);
    throw error;
  } finally {
    proc.stdin?.end();
  }
});
