import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

const repo = join(import.meta.dirname, "..");
const { chromium, webkit } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const browserType = process.env.BROWSER === "webkit" ? webkit : chromium;
const root = await mkdtemp(join(tmpdir(), "studio-subagent-limit-"));
const server = spawn(
  process.env.PYTHON || "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    env: {
      ...process.env,
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(
        ["gpt-6-astra", "gpt-6-luna", "gpt-5.6-sol", "gpt-5.6-luna"].map(
          (model) => ({
            model,
            defaultReasoningEffort: "low",
            supportedReasoningEfforts: [
              { reasoningEffort: "low" },
              { reasoningEffort: "high" },
              { reasoningEffort: "max" },
            ],
          }),
        ),
      ),
    },
    stdio: ["pipe", "pipe", "pipe"],
  },
);
let log = "",
  browser;
server.stderr.on("data", (chunk) => {
  log += chunk;
});
try {
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(Error(log || "Fixture timeout")),
      30000,
    );
    server.stdout.once("data", (chunk) => {
      clearTimeout(timer);
      resolve(Number(String(chunk).trim()));
    });
    server.once("exit", () => {
      clearTimeout(timer);
      reject(Error(log || "Fixture exited"));
    });
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = async () =>
    (await fetch(origin + "/api/state?view=chat")).json();
  const initial = await snapshot();
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  const other = initial.threads.find((agent) => agent.name === "Other project");
  assert(lead.agentModeSupported);
  const activeWorkers = initial.threads.filter(
    (agent) => agent.rootId === lead.id && agent.status === "running",
  );
  assert(activeWorkers.length > 0);

  assert.equal(typeof lead.concurrency, "number");
  const currentLead = async () =>
    (await snapshot()).threads.find((agent) => agent.id === lead.id);
  browser = await browserType.launch({
    headless: true,
    ...(browserType === chromium &&
    (process.env.CHROME_BIN || process.platform === "darwin")
      ? {
          executablePath:
            process.env.CHROME_BIN ||
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        }
      : {}),
  });
  const page = await browser.newPage({
    viewport: { width: 1280, height: 900 },
    serviceWorkers: "block",
  });
  page.setDefaultTimeout(15000);
  const errors = [],
    writes = [],
    prompts = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.addInitScript(
    ({ stateDir, id }) => {
      if (!localStorage.getItem(`codex-desktop-opened:${stateDir}`)) {
        localStorage.setItem(
          `codex-desktop-opened:${stateDir}`,
          JSON.stringify(id),
        );
        localStorage.setItem("codex-mobile-opened", JSON.stringify(id));
      }
    },
    { stateDir: initial.stateDir, id: lead.id },
  );
  let loseNextReply = false,
    raceNextRequest = false;
  await page.route("**/api/conversation", async (route) => {
    const body = route.request().postDataJSON();
    if (body.subagent_concurrency === undefined && !body.agent_mode)
      return route.continue();
    writes.push(body);
    if (raceNextRequest) {
      raceNextRequest = false;
      const response = await fetch(origin + "/api/conversation", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": initial.token,
        },
        body: JSON.stringify({
          id: body.id,
          subagent_concurrency: 8,
          expected_mode_revision: body.expected_mode_revision,
          request_id: "browser-conflicting-device",
        }),
      });
      assert.equal(response.status, 200);
      assert.equal((await response.json()).concurrency, 8);
    }
    if (loseNextReply) {
      loseNextReply = false;
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      return route.abort("failed");
    }
    return route.continue();
  });
  page.on("request", (request) => {
    if (
      request.method() === "POST" &&
      /\/api\/(send|create|action|stop)$/.test(new URL(request.url()).pathname)
    )
      prompts.push(request.url());
  });
  await page.goto(origin);

  const control = page.locator(".agent-mode-control");
  const input = page.getByRole("spinbutton", { name: "Subagent parallelism" });
  const apply = control.getByRole("button", { name: "Apply", exact: true });
  const waitConcurrency = (value) =>
    page.waitForFunction(
      (value) =>
        document
          .querySelector(".agent-mode-control")
          ?.getAttribute("data-concurrency") === String(value),
      value,
    );
  const waitRevision = (value) =>
    page.waitForFunction(
      (value) =>
        document
          .querySelector(".agent-mode-control")
          ?.getAttribute("data-agent-mode-revision") === String(value),
      value,
    );
  await input.waitFor();
  assert.equal(await input.inputValue(), "32");
  assert.equal(await control.getAttribute("data-agent-mode"), "multi");
  assert.equal(await input.getAttribute("min"), "0");
  assert.equal(await input.getAttribute("max"), "512");
  assert.equal(
    await input.getAttribute("title"),
    "0 disables subagents. The lead does not count. Extra work waits in the queue.",
  );
  const helpId = await input.getAttribute("aria-describedby");
  assert.equal(
    await page.locator(`#${helpId}`).textContent(),
    "0 disables subagents. The lead does not count. Extra work waits in the queue.",
  );
  await input.fill("64");
  await apply.click();
  await waitConcurrency(64);
  assert.deepEqual(Object.keys(writes.at(-1)).sort(), [
    "expected_mode_revision",
    "id",
    "request_id",
    "subagent_concurrency",
  ]);
  assert.equal(writes.at(-1).subagent_concurrency, 64);
  assert.equal(writes.at(-1).expected_mode_revision, 0);
  assert.equal((await currentLead()).concurrency, 64);

  // Zero expresses Single agent and does not interrupt accepted workers.
  await input.fill("0");
  await apply.click();
  await waitConcurrency(0);
  assert.equal(await control.getAttribute("data-agent-mode"), "single");
  await page
    .getByText("No new worker turns will start.", { exact: true })
    .waitFor();
  let state = await snapshot();
  assert.equal(
    state.threads.find((agent) => agent.id === lead.id).concurrency,
    0,
  );
  for (const worker of activeWorkers)
    assert.equal(
      state.threads.find((agent) => agent.id === worker.id).status,
      "running",
    );

  // Capacity at the product maximum remains available; positive values restore Multi.
  await input.fill("512");
  await apply.click();
  await waitConcurrency(512);
  assert.equal(await control.getAttribute("data-agent-mode"), "multi");
  assert.equal((await currentLead()).concurrency, 512);
  assert.deepEqual(prompts, []);
  console.log(
    "PASS numeric limit, 0/positive mode derivation, and active workers retained",
  );

  // A lost response keeps the same request identity across a reload and retry.
  loseNextReply = true;
  await input.fill("128");
  await apply.click();
  const retry = page.getByRole("button", {
    name: "Retry limit change",
    exact: true,
  });
  await retry.waitFor();
  const lost = writes.at(-1);
  assert.equal(lost.expected_mode_revision, 3);
  assert.equal((await currentLead()).agentModeRevision, 4);
  await page.reload();
  await retry.waitFor();
  await retry.click();
  await retry.waitFor({ state: "detached" });
  assert.deepEqual(writes.at(-1), lost);
  assert.equal((await currentLead()).agentModeRevision, 4);
  await waitConcurrency(128);
  assert.equal((await currentLead()).concurrency, 128);
  console.log(
    "PASS lost response, reload, and exact retry without duplicate apply",
  );

  // A durable pending operation from the old two-state UI remains exactly retryable.
  const legacy = {
    id: lead.id,
    agent_mode: "single",
    expected_mode_revision: 4,
    request_id: "legacy-mode-request-identity",
  };
  await page.evaluate(
    ({ leadId, legacy }) => {
      const key = Object.keys(localStorage).find(
        (key) =>
          key.startsWith("studio-agent-mode:") &&
          JSON.parse(key.slice("studio-agent-mode:".length))[2] === leadId,
      );
      if (!key) throw Error("No scoped setting record");
      localStorage.setItem(
        key,
        JSON.stringify({
          confirmed: { mode: "multi", revision: 4 },
          pending: legacy,
        }),
      );
    },
    { leadId: lead.id, legacy },
  );
  await page.reload();
  await page
    .getByRole("button", { name: "Retry mode change", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Retry mode change", exact: true })
    .waitFor({ state: "detached" });
  assert.deepEqual(writes.at(-1), legacy);
  await waitConcurrency(0);
  assert.equal((await currentLead()).concurrency, 0);
  console.log("PASS exact migration retry for a saved legacy mode request");

  // Conflict is explicit: discard that request, refresh canonical value, allow editing.
  raceNextRequest = true;
  await input.fill("16");
  await apply.click();
  await control.getByRole("alert").waitFor();
  await waitConcurrency(8);
  await waitRevision(6);
  await page.waitForFunction(
    () =>
      document.querySelector('.agent-mode-control input[type="number"]')
        ?.value === "8",
  );
  assert.equal(await retry.count(), 0);
  await input.fill("9");
  await apply.click();
  await waitConcurrency(9);
  assert.equal((await currentLead()).agentModeRevision, 7);
  assert.equal((await currentLead()).concurrency, 9);
  console.log(
    "PASS CAS conflict clears rejected request and refreshes canonical setting",
  );

  // No-op retry is valid at the same revision and causes no revision increment.
  const noOp = {
    id: lead.id,
    subagent_concurrency: 9,
    expected_mode_revision: 7,
    request_id: "same-revision-no-op",
  };
  await page.evaluate(
    ({ leadId, noOp }) => {
      const key = Object.keys(localStorage).find(
        (key) =>
          key.startsWith("studio-agent-mode:") &&
          JSON.parse(key.slice("studio-agent-mode:".length))[2] === leadId,
      );
      localStorage.setItem(
        key,
        JSON.stringify({
          confirmed: { concurrency: 9, revision: 7 },
          pending: noOp,
        }),
      );
    },
    { leadId: lead.id, noOp },
  );
  await page.reload();
  await page
    .getByRole("button", { name: "Retry limit change", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Retry limit change", exact: true })
    .waitFor({ state: "detached" });
  assert.equal((await currentLead()).agentModeRevision, 7);
  assert.deepEqual(writes.at(-1), noOp);
  await waitConcurrency(9);
  console.log("PASS same-revision no-op receipt is accepted");

  // Each chat keeps a separate limit.
  await page.locator(`[data-chat="${other.id}"]`).click();
  await page
    .locator("#conversation-title")
    .filter({ hasText: "Other project" })
    .waitFor();
  const otherInput = page.getByRole("spinbutton", {
    name: "Subagent parallelism",
  });
  await otherInput.fill("17");
  await page
    .locator(".agent-mode-control")
    .getByRole("button", { name: "Apply", exact: true })
    .click();
  await waitConcurrency(17);
  assert.equal(
    (await snapshot()).threads.find((agent) => agent.id === other.id)
      .concurrency,
    17,
  );
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await waitConcurrency(9);
  console.log("PASS settings are isolated per chat");

  // An unreadable browser record stays visible and can be discarded without a command.
  await page.evaluate((id) => {
    const key = Object.keys(localStorage).find(
      (key) =>
        key.startsWith("studio-agent-mode:") &&
        JSON.parse(key.slice("studio-agent-mode:".length))[2] === id,
    );
    localStorage.setItem(key, "{bad json");
  }, lead.id);
  await page.reload();
  const discard = page.getByRole("button", {
    name: "Discard unreadable concurrency request",
    exact: true,
  });
  await discard.waitFor();
  assert.equal(await input.isEnabled(), false);
  const beforeDiscard = writes.length;
  await discard.click();
  await discard.waitFor({ state: "detached" });
  assert.equal(await input.isEnabled(), true);
  assert.equal(writes.length, beforeDiscard);

  // A command is never sent unless its request identity can be durably stored.
  const beforeStorageFailure = writes.length;
  await page.evaluate(() => {
    window.limitStorageSetItem = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (key.startsWith("studio-agent-mode:"))
        throw new DOMException("Full", "QuotaExceededError");
      return window.limitStorageSetItem.call(this, key, value);
    };
  });
  await input.fill("10");
  await apply.click();
  await control.getByRole("alert").waitFor();
  assert.equal(writes.length, beforeStorageFailure);
  assert.equal(await input.isEnabled(), true);
  await page.evaluate(() => {
    Storage.prototype.setItem = window.limitStorageSetItem;
  });

  // Mobile exposes the same numeric control in Chat settings with no horizontal overflow.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole("button", { name: "Chat actions", exact: true }).click();
  await page
    .getByRole("menuitem", { name: "Chat settings", exact: true })
    .click();
  await input.waitFor();
  assert(
    await control.evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
  );
  assert(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  );
  const box = await input.boundingBox();
  assert(box && box.x >= 0 && box.x + box.width <= 390 && box.height >= 36);
  await input.fill("512");
  await control.getByRole("button", { name: "Apply", exact: true }).click();
  await waitConcurrency(512);
  assert.equal((await currentLead()).concurrency, 512);
  assert.deepEqual(prompts, []);
  assert.deepEqual(errors, []);
  await page.screenshot({ path: join(root, "mobile-subagent-limit.png") });
  console.log(
    JSON.stringify({
      result: "PASS",
      browser: browserType.name(),
      evidence: root,
      writes: writes.length,
      prompts: prompts.length,
    }),
  );
} finally {
  await browser?.close();
  server.kill("SIGTERM");
  if (server.exitCode === null && server.signalCode === null)
    await once(server, "exit").catch(() => {});
}
