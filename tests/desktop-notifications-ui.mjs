// Production UI with an isolated backend and a recorded native bridge. No OS alerts.
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
const evidence = await mkdtemp(join(tmpdir(), "studio-desktop-alerts-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  page,
  log = "";
fixture.stderr.on("data", (value) => {
  log += value;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (value) =>
      resolve(Number(String(value).trim())),
    );
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  let snapshot = await (await fetch(origin + "/api/state?view=chat")).json();
  const response = await fetch(origin + "/api/leads", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": snapshot.token,
    },
    body: JSON.stringify({
      name: "Other project",
      cwd: evidence,
      request_id: "notification-fixture-lead",
    }),
  });
  assert.equal(response.ok, true, await response.text());
  snapshot = await (await fetch(origin + "/api/state?view=chat")).json();
  const lead = snapshot.threads.find((a) => a.name === "Release lead");
  const other = snapshot.threads.find((a) => a.name === "Other project");
  assert.ok(other);
  const change = (id, fields) => {
    for (const rows of [snapshot.threads, snapshot.runtime.agents])
      Object.assign(
        rows.find((a) => a.id === id),
        fields,
      );
  };
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  page.setDefaultTimeout(12000);
  await page.addInitScript(
    ({ stateDir, id }) => {
      localStorage.setItem(
        `codex-desktop-opened:${stateDir}`,
        JSON.stringify(id),
      );
      localStorage.setItem("workspace-notifications", "false");
      window.alerts = [];
      window.fakeFocus = false;
      document.hasFocus = () => window.fakeFocus;
      window.codexDesktop = {
        platform: "darwin",
        notify: async (value) => {
          window.alerts.push(value);
          return true;
        },
        onNavigate: (callback) => {
          window.navigateAlert = callback;
          return () => {};
        },
      };
    },
    { stateDir: snapshot.stateDir, id: lead.id },
  );
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/sync/**", (route) =>
    route.fulfill({ status: 404, json: { error: "HTTP fixture" } }),
  );
  let reads = 0;
  await page.route(/\/api\/state(?:\?.*)?$/, (route) => {
    reads++;
    return route.fulfill({ json: snapshot });
  });
  await page.goto(origin);
  await page
    .locator("#conversation-title")
    .getByText("Release lead", { exact: true })
    .waitFor();
  const refresh = async () => {
    const before = reads;
    await page.evaluate(() => window.dispatchEvent(new Event("focus")));
    const deadline = Date.now() + 12000;
    while (reads <= before && Date.now() < deadline)
      await page.waitForTimeout(100);
    assert.ok(reads > before, "a new snapshot arrived");
    await page.waitForTimeout(250);
  };
  assert.equal(
    await page.evaluate(() => window.alerts.length),
    0,
    "old events are a baseline",
  );
  change(other.id, {
    status: "completed",
    inFlight: false,
    lastCompletedTurn: "notify-1",
    lastCompletedTurnStatus: "completed",
    tail: "Other project checks passed.",
  });
  await refresh();
  await page.waitForFunction(() => window.alerts.length === 1);
  const first = await page.evaluate(() => window.alerts[0]);
  assert.equal(first.target.agentId, other.id);
  assert.equal(first.body, "Other project checks passed.");
  await refresh();
  assert.equal(
    await page.evaluate(() => window.alerts.length),
    1,
    "no duplicate snapshot alert",
  );
  await page.evaluate(() => window.navigateAlert(window.alerts[0].target));
  await page
    .locator("#conversation-title")
    .getByText("Other project", { exact: true })
    .waitFor();
  assert.equal(
    await page.getByRole("dialog").count(),
    0,
    "completion opens the conversation",
  );
  await page.evaluate(() => {
    window.fakeFocus = true;
  });
  change(other.id, { lastCompletedTurn: "notify-2" });
  await refresh();
  assert.equal(
    await page.evaluate(() => window.alerts.length),
    1,
    "focused current chat is silent",
  );
  change(lead.id, {
    status: "completed",
    inFlight: false,
    lastCompletedTurn: "notify-3",
    lastCompletedTurnStatus: "completed",
  });
  await refresh();
  await page.waitForFunction(() => window.alerts.length === 2);
  const worker = snapshot.threads.find((a) => !a.isLead);
  change(worker.id, {
    status: "completed",
    inFlight: false,
    lastCompletedTurn: "worker-done",
    lastCompletedTurnStatus: "completed",
  });
  await refresh();
  assert.equal(
    await page.evaluate(() => window.alerts.length),
    2,
    "worker completion is silent",
  );
  snapshot.runtime.requests.push({
    id: "new-question",
    agent: lead.id,
    status: "pending",
    method: "agent/asyncQuestion",
    params: { questions: [{ id: "scope", question: "Which release scope?" }] },
  });
  await refresh();
  await page.waitForFunction(() => window.alerts.length === 3);
  assert.equal(
    await page.evaluate(() => window.alerts[2].target.itemId),
    "new-question",
  );
  await page.evaluate(() => window.navigateAlert(window.alerts[2].target));
  await page.getByRole("dialog").waitFor();
  await page
    .getByRole("dialog")
    .getByText("Which release scope?", { exact: true })
    .waitFor();
  await page.screenshot({ path: join(evidence, "notification-question.png") });
  await page.evaluate(() => {
    window.fakeFocus = false;
    window.deniedErrors = [];
    window.addEventListener("desktop-error", (event) =>
      window.deniedErrors.push(event.detail),
    );
    window.codexDesktop.notify = async () => {
      throw new Error(
        "Error invoking remote method 'codex-desktop': Error: Notifications are not allowed for this application",
      );
    };
  });
  change(lead.id, { lastCompletedTurn: "denied-once" });
  await refresh();
  await page.waitForFunction(() => window.deniedErrors.length === 1);
  await refresh();
  change(lead.id, { lastCompletedTurn: "denied-twice" });
  await refresh();
  assert.equal(
    await page.evaluate(() => window.deniedErrors.length),
    1,
    "permission denial is shown once, not retried on every snapshot",
  );
  assert.match(
    await page.evaluate(() => window.deniedErrors[0]),
    /Enable Allow notifications for Codex Studio/,
  );
  assert.doesNotMatch(
    await page.evaluate(() => window.deniedErrors[0]),
    /remote method/,
  );

  assert.equal(
    await page.getByRole("button", { name: /desktop alerts/ }).count(),
    0,
  );
  await page.evaluate(() => {
    window.codexDesktop.notify = async (value) => {
      window.alerts.push(value);
      return true;
    };
  });
  change(other.id, { lastCompletedTurn: "global-turn" });
  await refresh();
  await page.waitForFunction(() => window.alerts.length === 4);
  await page.reload();
  await page.locator("#conversation-title").waitFor();
  await refresh();
  assert.equal(
    await page.evaluate(() => window.alerts.length),
    0,
    "reload does not repeat old alerts",
  );
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ passed: true, evidence }));
} catch (error) {
  await page?.screenshot({ path: join(evidence, "failure.png") });
  console.error(evidence, log);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
