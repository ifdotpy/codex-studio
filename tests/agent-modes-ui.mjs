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
const root = await mkdtemp(join(tmpdir(), "studio-agent-modes-"));
const server = spawn(
  process.env.PYTHON || "/opt/homebrew/bin/python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    env: {
      ...process.env,
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(
        ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-luna"].map((model) => ({
          model,
          defaultReasoningEffort: "low",
          supportedReasoningEfforts: [
            { reasoningEffort: "low" },
            { reasoningEffort: "max" },
          ],
        })),
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
  const lead = initial.threads.find((a) => a.name === "Release lead");
  const other = initial.threads.find((a) => a.name === "Other project");
  assert(lead.agentModeSupported);
  const activeWorkers = initial.threads.filter(
    (a) => a.rootId === lead.id && a.status === "running",
  );
  assert(activeWorkers.length > 0);
  browser = await browserType.launch({
    headless: true,
    ...(browserType === chromium
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
    delayNextReply = false,
    staleNextRequest = false,
    delayed;
  await page.route("**/api/conversation", async (route) => {
    const body = route.request().postDataJSON();
    if (!body.agent_mode) return route.continue();
    writes.push(body);
    if (staleNextRequest) {
      staleNextRequest = false;
      const state = await snapshot();
      const response = await fetch(origin + "/api/conversation", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Canvas-Token": state.token,
        },
        body: JSON.stringify({ ...body, request_id: "mode-competing-device" }),
      });
      assert.equal(response.status, 200);
    }
    if (loseNextReply || delayNextReply) {
      const lose = loseNextReply;
      loseNextReply = false;
      delayNextReply = false;
      const response = await route.fetch();
      assert.equal(response.status(), 200);
      if (lose) return route.abort("failed");
      delayed = () => route.fulfill({ response });
      return;
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
  const modeSwitch = page.getByRole("switch", { name: "Multi-agent mode" });
  const waitMode = async (mode) =>
    page.waitForFunction(
      (mode) =>
        document
          .querySelector(".agent-mode-control")
          ?.getAttribute("data-agent-mode") === mode,
      mode,
    );
  await modeSwitch.waitFor();
  assert.equal(await modeSwitch.getAttribute("aria-checked"), "true");
  assert.equal(await modeSwitch.isEnabled(), true);
  await modeSwitch.click();
  await waitMode("single");
  await page.getByText("Current work can finish.", { exact: true }).waitFor();
  let state = await snapshot();
  assert.equal(
    state.threads.find((a) => a.id === lead.id).agentModeRevision,
    1,
  );
  for (const worker of activeWorkers)
    assert.equal(
      state.threads.find((a) => a.id === worker.id).status,
      "running",
    );
  await modeSwitch.click();
  await waitMode("multi");
  assert.equal(
    (await snapshot()).threads.find((a) => a.id === lead.id).agentModeRevision,
    2,
  );
  console.log(
    "PASS busy team switch, accepted workers preserved, reverse switch",
  );

  // A different idle chat has its own mode and receipt scope.
  await page.locator(`[data-chat="${other.id}"]`).click();
  await page
    .locator("#conversation-title")
    .filter({ hasText: "Other project" })
    .waitFor();
  await modeSwitch.click();
  await waitMode("single");
  assert.equal(
    (await snapshot()).threads.find((a) => a.id === other.id).agentModeRevision,
    1,
  );
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await waitMode("multi");
  loseNextReply = true;
  await modeSwitch.click();
  const retry = page.getByRole("button", {
    name: "Retry mode change",
    exact: true,
  });
  await retry.waitFor();
  const lost = writes.at(-1);
  assert.equal(lost.expected_mode_revision, 2);
  assert.equal(
    (await snapshot()).threads.find((a) => a.id === lead.id).agentModeRevision,
    3,
  );
  await page.locator(`[data-chat="${other.id}"]`).click();
  await waitMode("single");
  assert.equal(await retry.count(), 0);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await retry.waitFor();
  await page.reload();
  await retry.waitFor();
  await retry.click();
  await retry.waitFor({ state: "detached" });
  assert.deepEqual(writes.at(-1), lost);
  assert.equal(
    (await snapshot()).threads.find((a) => a.id === lead.id).agentModeRevision,
    3,
  );
  await waitMode("single");
  console.log(
    "PASS idle chat, receipt scope, lost response, reload and exact retry without duplicate change",
  );

  // A late receipt must not replace the newer canonical mode from another device.
  delayNextReply = true;
  await modeSwitch.click();
  await page.waitForFunction(
    () =>
      document.querySelector(".agent-mode-control [role=status]")
        ?.textContent === "Applying…",
  );
  for (let i = 0; !delayed && i < 200; i++)
    await new Promise((resolve) => setTimeout(resolve, 25));
  assert(delayed);
  state = await snapshot();
  assert.equal(
    state.threads.find((a) => a.id === lead.id).agentModeRevision,
    4,
  );
  const remote = await fetch(origin + "/api/conversation", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": state.token,
    },
    body: JSON.stringify({
      id: lead.id,
      agent_mode: "single",
      expected_mode_revision: 4,
      request_id: "mode-other-device",
    }),
  });
  assert.equal(remote.status, 200);
  await page.waitForFunction(
    () =>
      document
        .querySelector(".agent-mode-control")
        ?.getAttribute("data-agent-mode-revision") === "5",
  );
  await delayed();
  await page.waitForFunction(
    () => !document.querySelector(".agent-mode-control [role=status]"),
  );
  await waitMode("single");
  assert.equal(await control.getAttribute("data-agent-mode-revision"), "5");
  await page.reload();
  await waitMode("single");
  assert.equal(await control.getAttribute("data-agent-mode-revision"), "5");
  console.log(
    "PASS delayed receipt cannot replace a newer mode, revision survives reload",
  );

  staleNextRequest = true;
  await modeSwitch.click();
  await control.getByRole("alert").waitFor();
  await waitMode("multi");
  assert.equal(await retry.count(), 0);
  assert.equal(await modeSwitch.isEnabled(), true);
  await modeSwitch.click();
  await waitMode("single");
  assert.equal(await control.getByRole("alert").count(), 0);
  console.log(
    "PASS stale revision rejection clears the receipt and allows the next change",
  );

  // A malformed browser record remains visible until explicitly discarded.
  await page.evaluate((id) => {
    const key = Object.keys(localStorage).find(
      (key) =>
        key.startsWith("studio-agent-mode:") &&
        key.includes(id) &&
        JSON.parse(key.slice("studio-agent-mode:".length))[1],
    );
    if (!key) throw Error("No scoped mode record");
    localStorage.setItem(key, "{bad json");
  }, lead.id);
  await page.reload();
  const discard = page.getByRole("button", {
    name: "Discard unreadable mode request",
    exact: true,
  });
  await discard.waitFor();
  assert.equal(await modeSwitch.isEnabled(), false);
  const beforeDiscard = writes.length;
  await discard.click();
  await discard.waitFor({ state: "detached" });
  assert.equal(await modeSwitch.isEnabled(), true);
  assert.equal(writes.length, beforeDiscard);
  await waitMode("single");
  console.log(
    "PASS unreadable receipt remains visible and can be discarded without a command",
  );

  const beforeStorageFailure = writes.length;
  await page.evaluate(() => {
    window.modeStorageSetItem = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (key.startsWith("studio-agent-mode:"))
        throw new DOMException("Full", "QuotaExceededError");
      return window.modeStorageSetItem.call(this, key, value);
    };
  });
  await modeSwitch.click();
  await control
    .getByRole("alert")
    .filter({ hasText: "Cannot save the mode request" })
    .waitFor();
  assert.equal(writes.length, beforeStorageFailure);
  assert.equal(await modeSwitch.isEnabled(), true);
  await page.evaluate(() => {
    Storage.prototype.setItem = window.modeStorageSetItem;
  });
  console.log("PASS browser storage failure prevents an unrepeatable command");

  await page.setViewportSize({ width: 390, height: 844 });
  await modeSwitch.waitFor();
  assert.equal(await modeSwitch.isEnabled(), true);
  assert(
    await control.evaluate((node) => node.scrollWidth <= node.clientWidth + 1),
  );
  assert(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  );
  const box = await modeSwitch.boundingBox();
  assert(box && box.x >= 0 && box.x + box.width <= 390 && box.height >= 36);
  await page.screenshot({ path: join(root, "mobile-mode.png") });
  await modeSwitch.click();
  await waitMode("multi");
  assert.deepEqual(prompts, []);
  assert.deepEqual(errors, []);
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
