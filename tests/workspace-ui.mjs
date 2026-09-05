#!/usr/bin/env node
// Exercise workspace actions through the real runtime and HTTP server.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(skill, "web/package.json"))(
  "playwright-core",
);
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
  "# Release evidence\nVerified report for orchestration.\n",
);
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
  },
);
let log = "",
  browser,
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
  const initial = await get("/api/state");
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
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  page = await browser.newPage({ viewport: { width: 1440, height: 980 } });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(origin);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.locator("#workspace-toggle").click();
  const drawer = page.locator(".workspace-drawer .mantine-Drawer-content");
  const section = async (name) => {
    await page
      .getByRole("navigation", { name: "Workspace sections" })
      .getByRole("button", { name, exact: true })
      .click();
  };
  await drawer.getByRole("button", { name: "New work", exact: true }).click();
  let modal = page.locator(".mantine-Modal-content:visible").last();
  await modal.getByLabel("Title").fill("Verify release");
  await modal
    .getByLabel("Description")
    .fill("Verify all release evidence and produce a report.");
  await modal.getByLabel("Owner").selectOption({ label: "Worker 39" });
  await modal.getByRole("button", { name: "Save work", exact: true }).click();
  await poll(
    async () => (await get("/api/work?agent=" + lead.id)).tasks?.length === 1,
    "work persisted",
  );
  await drawer
    .getByRole("button")
    .filter({ hasText: "Verify release" })
    .click();
  await drawer
    .getByRole("button", { name: "Claim for owner", exact: true })
    .click();
  await poll(
    async () =>
      (await get("/api/work?agent=" + lead.id)).tasks[0].status === "running",
    "work claimed",
  );
  await drawer
    .getByRole("button", { name: "Submit result", exact: true })
    .click();
  modal = page.locator(".mantine-Modal-content:visible").last();
  await modal.getByLabel("Result").fill("All release checks pass.");
  await modal
    .getByLabel("Checks and evidence")
    .fill("runtime-contract passes; report attached.");
  await modal.getByLabel("Revision or artifact identity").fill(
    execFileSync("git", ["rev-parse", "HEAD"], {
      cwd: root,
      encoding: "utf8",
    }).trim(),
  );
  await modal.getByLabel("Report files").fill("report.md");
  await modal
    .getByRole("button", { name: "Submit for acceptance", exact: true })
    .click();
  await poll(
    async () =>
      (await get("/api/work?agent=" + lead.id)).tasks[0].status === "review",
    "submitted result",
  );
  await drawer.getByText("All release checks pass.", { exact: true }).waitFor();
  await drawer.getByRole("button", { name: "report.md", exact: true }).click();
  await page
    .getByRole("dialog")
    .last()
    .getByText("Verified report for orchestration.", { exact: false })
    .waitFor();
  await page
    .getByRole("dialog")
    .last()
    .getByRole("button", { name: "Close", exact: true })
    .click();
  await drawer.getByRole("button", { name: "Accept", exact: true }).click();
  modal = page.locator(".mantine-Modal-content:visible").last();
  await modal
    .getByLabel("Decision and evidence")
    .fill("Reviewed the report and the recorded checks.");
  await modal
    .getByRole("button", { name: "Accept result", exact: true })
    .click();
  await poll(
    async () =>
      (await get("/api/work?agent=" + lead.id)).tasks[0].status === "accepted",
    "accepted result",
  );
  await page
    .locator(".mantine-Modal-content:visible")
    .waitFor({ state: "hidden" });
  await page.screenshot({
    path: join(root, "work-desktop.png"),
    fullPage: true,
  });
  await section("Plan");
  await drawer
    .getByLabel("Shared plan")
    .fill("1. Inspect evidence\n2. Accept the release");
  await drawer.getByRole("button", { name: "Save plan", exact: true }).click();
  await poll(
    async () =>
      (await get("/api/plan?agent=" + lead.id)).text.includes(
        "Accept the release",
      ),
    "plan persisted",
  );
  await section("Search");
  await drawer
    .getByRole("textbox", { name: "Search all conversations" })
    .fill("Verify release");
  await drawer
    .getByRole("button", { name: "Search", exact: true })
    .last()
    .click();
  await drawer.getByText("Verify release", { exact: true }).waitFor();
  await drawer.getByText("Verify release", { exact: true }).click();
  modal = page.locator(".mantine-Modal-content:visible").last();
  await modal.getByText("All release checks pass.", { exact: false }).waitFor();
  await modal.getByRole("button", { name: "Open work", exact: true }).click();
  await drawer
    .getByRole("heading", { name: "Verify release", exact: true })
    .waitFor();
  await section("Rules");
  await drawer.getByRole("button", { name: "New rule", exact: true }).click();
  modal = page.locator(".mantine-Modal-content:visible").last();
  await modal.getByLabel("Name").fill("Release watch");
  await modal
    .getByLabel("Message to the agent")
    .fill("Check release evidence.");
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
  await modal
    .getByRole("button", { name: "Save profile", exact: true })
    .click();
  await poll(
    async () => (await get("/api/profiles")).profiles.length === 1,
    "profile persisted",
  );
  await drawer
    .getByRole("button", { name: "Start worker", exact: true })
    .click();
  modal = page.locator(".mantine-Modal-content:visible").last();
  await modal
    .getByLabel("Task for this worker")
    .fill("Review the release evidence using the saved profile.");
  await modal
    .getByRole("button", { name: "Start worker", exact: true })
    .click();
  await poll(
    async () =>
      (await get("/api/state")).runtime.agents.some(
        (a) =>
          a.name === "Evidence reviewer" &&
          a.parentId === lead.id &&
          a.profileId,
      ),
    "profile worker created in selected team",
  );
  await section("Resources");
  await drawer.getByLabel("Resource").fill("fixture-build-slot");
  await drawer
    .getByLabel("Purpose", { exact: true })
    .fill("Workspace browser test");
  await drawer.getByRole("button", { name: "Claim", exact: true }).click();
  await poll(
    async () =>
      (await get("/api/resources")).state.claims["fixture-build-slot"]
        ?.worker === lead.id,
    "resource claimed in isolated board",
  );
  await drawer.getByRole("button", { name: "Renew", exact: true }).click();
  await drawer.getByRole("button", { name: "Release", exact: true }).click();
  await poll(
    async () =>
      !(await get("/api/resources")).state.claims["fixture-build-slot"],
    "resource released",
  );
  await section("Changes");
  await drawer.getByRole("region", { name: "Changes diff" }).waitFor();
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
      (await get("/api/workspace?agent=" + lead.id)).annotations.length === 1,
    "line comment stored",
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
    .getByText("A restore requires an idle agent with an isolated worktree.", {
      exact: true,
    })
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
  for (const name of ["Tools", "Resources", "Inbox"]) {
    await section(name);
    await drawer.getByRole("heading", { name, exact: true }).waitFor();
  }
  await section("Work");
  await drawer
    .getByRole("button")
    .filter({ hasText: "Verify release" })
    .click();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({
    path: join(root, "work-mobile.png"),
    fullPage: true,
  });
  assert.ok(
    await drawer.evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
    "drawer does not overflow at 390px",
  );
  for (const name of [
    "Changes",
    "Plan",
    "Profiles",
    "Rules",
    "Resources",
    "Inbox",
  ]) {
    await section(name);
    assert.ok(
      await drawer.evaluate((el) => el.scrollWidth <= el.clientWidth + 1),
      name + " no mobile overflow",
    );
  }
  assert.deepEqual(errors, []);
  console.log(
    "PASS workspace UI: work lifecycle, report, plan, search, rule lifecycle, profile launch, isolated resource leases, exact search source, line comment, safe HTML preview, checkpoint preview, 390px sections. Evidence: " +
      root,
  );
} catch (error) {
  if (browser) {
    const pages = browser.contexts().flatMap((c) => c.pages());
    if (pages[0])
      await pages[0].screenshot({
        path: join(root, "failure.png"),
        fullPage: true,
      });
  }
  console.error("Evidence:", root);
  throw error;
} finally {
  if (browser) await browser.close();
  proc.kill("SIGTERM");
}
