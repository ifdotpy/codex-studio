#!/usr/bin/env node
// Production renderer with an isolated fixture. No model calls or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-worker-overview-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, CODEX_BOARD_STATE_DIR: join(root, "board") },
  },
);
let browser,
  page,
  log = "";
proc.stderr.on("data", (data) => (log += data));
try {
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(Error("Fixture timeout\n" + log)),
      30000,
    );
    proc.stdout.once("data", (data) => {
      clearTimeout(timer);
      resolve(Number(String(data).trim()));
    });
    proc.once("exit", () => {
      clearTimeout(timer);
      reject(Error(log));
    });
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await (await fetch(origin + "/api/state")).json();
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  const workers = initial.threads.filter(
    (agent) => agent.rootId === lead.id && !agent.isLead,
  );
  const worker = (n) =>
    workers.find(
      (agent) => agent.name === `Worker ${String(n).padStart(2, "0")}`,
    );
  const task =
    "Check the exact response receipt before any repeated mutation. " +
    "This is a long assignment with complete instructions. ".repeat(14).trim();
  const report =
    "Verified receipt recovery without duplicate work. " +
    "All observed cases and their evidence remain in the report. "
      .repeat(10)
      .trim();
  const longName =
    "Review the continuation receipt across concurrent requests and uncertain responses";
  let pending = true;
  let deferred = false;
  let transcriptRequests = 0;
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1440, height: 980 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    if (
      /\/api\/(thread|conversation|messages|transcript)/.test(
        new URL(request.url()).pathname,
      )
    )
      transcriptRequests++;
  });
  // Keep the status fixture on HTTP snapshots; sync has separate coverage.
  await page.route("**/api/sync/**", (route) =>
    route.fulfill({ status: 503, body: "Fixture uses HTTP snapshots" }),
  );
  await page.route("**/api/state", async (route) => {
    const response = await route.fetch();
    const data = await response.json();
    for (const agent of data.threads) {
      const workerIndex = workers.findIndex((worker) => worker.id === agent.id);
      if (workerIndex >= 0) {
        const index = Number(workers[workerIndex].name.split(" ")[1]);
        agent.status =
          index === 7
            ? "failed"
            : index < 8
              ? "running"
              : index < 25
                ? "queued"
                : "completed";
      }
      if (agent.id === worker(0).id && deferred) agent.status = "approval";
      if (agent.id === worker(1).id)
        Object.assign(agent, {
          name: longName,
          overview: { task, result: "" },
        });
      if (agent.id === worker(25).id)
        Object.assign(agent, {
          overview: {
            task: "Verify receipt recovery",
            result: report,
            resultTruncated: true,
          },
        });
    }
    if (pending)
      data.runtime.requests.push({
        id: "worker-question",
        agent: worker(0).id,
        status: "pending",
        deferred,
        method: "agent/asyncQuestion",
        params: {
          questions: [
            { id: "scope", question: "Which receipt should I inspect?" },
          ],
        },
      });
    data.runtime.requests.push({
      id: "answered-worker-question",
      agent: worker(2).id,
      status: "answered",
      method: "agent/asyncQuestion",
      params: { questions: [{ id: "done", question: "Already answered" }] },
    });
    await route.fulfill({ response, json: data });
  });
  await page.goto(origin);
  const selectLead = () =>
    page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await selectLead();
  assert.equal(
    await page.locator("#team-toggle").getAttribute("aria-expanded"),
    "false",
  );
  await page.locator("#team-toggle").click();
  const team = page.getByRole("complementary", { name: "Team", exact: true });
  const summary = team.getByLabel("Team status summary");
  const count = async (name) =>
    Number(await summary.locator(`[data-team-count="${name}"] dd`).innerText());
  const card = (n) =>
    team.locator(`[data-worker="${worker(n).id}"]`).locator("..");
  assert.equal(await count("working"), 6);
  assert.equal(await count("answer"), 1);
  assert.equal(await count("completed"), 15);
  assert.match(await summary.innerText(), /17 waiting · 1 need attention/);
  assert.equal(
    await team
      .getByRole("region", { name: "Attention", exact: true })
      .locator("[data-worker]")
      .count(),
    2,
  );
  assert.match(await card(0).innerText(), /Needs your answer/);
  const excerpt = card(1).locator(".worker-excerpt");
  assert.equal(
    await excerpt.locator(".worker-excerpt-preview").innerText(),
    task,
  );
  assert.equal(await excerpt.getAttribute("open"), null);
  const closedLayout = await excerpt
    .locator(".worker-excerpt-preview")
    .evaluate((node) => ({
      height: node.clientHeight,
      line: parseFloat(getComputedStyle(node).lineHeight),
    }));
  assert.ok(
    closedLayout.height <= closedLayout.line * 2 + 1,
    "the preview stays compact",
  );
  const beforeDisclosure = transcriptRequests;
  await excerpt.locator("summary").click();
  assert.equal(
    await excerpt.locator(".worker-excerpt-full p").innerText(),
    task,
  );
  assert.equal(
    await page.locator("#conversation-title").innerText(),
    "Release lead",
    "disclosure does not navigate",
  );
  assert.equal(
    transcriptRequests,
    beforeDisclosure,
    "card disclosure does not fetch a transcript",
  );
  assert.equal(
    await excerpt.evaluate((node) => node.scrollWidth <= node.clientWidth),
    true,
  );
  await excerpt.locator("summary").click();
  const search = team.getByRole("searchbox", { name: "Find a subagent" });
  await search.fill("Verified receipt recovery");
  await card(25).waitFor({ state: "visible" });
  assert.equal(
    await team.locator("[data-worker]").count(),
    1,
    "reports are searchable",
  );
  const result = card(25)
    .locator(".worker-excerpt")
    .filter({ hasText: "Last report" });
  assert.equal(
    await result.locator(".worker-excerpt-preview").innerText(),
    report,
  );
  await result.locator("summary").click();
  assert.equal(
    await result.locator(".worker-excerpt-full p").innerText(),
    report,
  );
  await result.getByRole("button", { name: "Continue in chat" }).click();
  assert.equal(
    await page.locator("#conversation-title").innerText(),
    worker(25).name,
  );
  await page
    .getByRole("button", { name: "Back to main agent", exact: true })
    .click();
  await search.fill("");
  deferred = true;
  await page.reload();
  await selectLead();
  assert.equal(
    await count("answer"),
    0,
    "deferred questions do not remind the user",
  );
  assert.equal(
    await count("working"),
    6,
    "a blocked deferred question is not working",
  );
  assert.match(await card(0).innerText(), /Question deferred/);
  pending = false;
  deferred = false;
  await page.reload();
  await selectLead();
  assert.equal(await count("answer"), 0, "answered requests stop counting");
  assert.equal(await count("working"), 7);
  await page.screenshot({ path: join(root, "worker-overview-desktop.png") });
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  assert.equal(
    await team.count(),
    0,
    "another chat cannot show the team summary",
  );
  await selectLead();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator("#team-toggle").click();
  await search.waitFor({ state: "visible" });
  await search.fill("exact response receipt");
  await card(1).waitFor({ state: "visible" });
  await card(1).locator(".worker-excerpt summary").click();
  assert.equal(
    await card(1).evaluate((node) => node.scrollWidth <= node.clientWidth),
    true,
  );
  assert.equal(await card(1).locator("strong").innerText(), longName);
  await page.screenshot({ path: join(root, "worker-overview-mobile.png") });
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      passed: true,
      summary: true,
      actualExcerpts: true,
      disclosure: true,
      noTranscriptFanout: true,
      search: true,
      chatIsolation: true,
      narrowLayout: true,
      screenshots: root,
    }),
  );
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Failure evidence:", root);
  throw error;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null)
    await new Promise((resolve) => proc.once("exit", resolve));
}
