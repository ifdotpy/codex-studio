#!/usr/bin/env node
// Production React build with isolated account fixtures. No credentials or model calls.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, extname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(root, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "codex-accounts-ui-"));
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
let defaultAccountKey = "default";
const logins = [];
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
  cwd: "/Users/igor/Projects/lumina",
});
const agents = [
  makeLead("started", "Started conversation", "default", false),
  makeLead("empty", "New conversation", "default", true),
];
const bodies = [];
let delayed = null;
let delayWork = false;
let wrongLimitsAccount = false;
let discoverCount = 0;
let limitReads = 0;
let delayCostWork = false;
let delayedCosts;
let snapshotLimits = {};
let claudeQueue = [];
const claudeSession = {
  version: "2.1.fixture",
  settings: { permissionMode: "default", thinking: true },
  turns: [
    { id: "claude-turn-one", text: "Original request", status: "completed" },
    { id: "claude-turn-two", text: "Later request", status: "completed" },
  ],
};
let failClaudeRollback = true;
let failClaudeCommand = true;
const transferReceipts = new Map();
let loseTransferResponse = true;
const limits = (key) => {
  if (key === "claude") {
    const main = {
      limitId: "claude",
      limitName: "Claude",
      planType: "max",
      primary: { usedPercent: 11, windowDurationMins: 300 },
      secondary: { usedPercent: 4, windowDurationMins: 10080 },
    };
    return {
      accountKey: key,
      at: Date.now() / 1000,
      data: {
        accountId: "native-claude",
        rateLimits: main,
        rateLimitsByLimitId: {
          "claude-fable": {
            limitId: "claude-fable",
            limitName: "Fable",
            secondary: { usedPercent: 7, windowDurationMins: 10080 },
          },
          claude: main,
        },
      },
    };
  }
  const codex = {
    limitId: "codex",
    planType: "pro",
    primary: {
      usedPercent: key === "default" ? 11 : key === "work" ? 22 : 33,
      windowDurationMins: 300,
      resetsAt: Date.now() / 1000 + 3600,
    },
    secondary: {
      usedPercent: 35,
      windowDurationMins: 10080,
      resetsAt: Date.now() / 1000 + 172800,
    },
  };
  return {
    accountKey: key,
    at: Date.now() / 1000,
    data: {
      accountId: `native-${key}`,
      rateLimits: codex,
      rateLimitsByLimitId: {
        codex,
        codex_bengalfox: {
          limitId: "codex_bengalfox",
          limitName: "GPT-5.3-Codex-Spark",
          primary: {
            ...codex.primary,
            usedPercent: 6,
            ...(key === "other" ? { resetsAt: Date.now() / 1000 - 60 } : {}),
          },
          secondary: {
            ...codex.secondary,
            usedPercent: key === "other" ? null : 15,
          },
        },
      },
      rateLimitResetCredits: {
        availableCount: 1,
        credits: [
          {
            id: `credit-${key}`,
            title: "Full reset",
            status: "available",
            resetType: "codexRateLimits",
          },
        ],
      },
    },
  };
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
  if (url.pathname === "/api/state")
    return json({
      token: "fixture",
      stateDir: evidence,
      threads: agents,
      chats: [],
      runtime: {
        agents,
        rooms: [],
        complaints: [],
        requests: [],
        monitors: [],
        tasks: [],
        work: [],
        userTasks: [],
        rateLimitsByAccount: snapshotLimits,
      },
    });
  if (
    url.pathname === "/api/accounts" ||
    url.pathname === "/api/accounts/discover" ||
    url.pathname === "/api/accounts/default"
  ) {
    if (url.pathname.endsWith("/default")) defaultAccountKey = body.account_key;
    if (url.pathname.endsWith("/discover")) discoverCount++;
    for (const receipt of logins) {
      if (
        accounts.find((a) => a.id === receipt.accountKey)?.status === "ready"
      ) {
        receipt.status = "ready";
        receipt.resolvedAccountKey = receipt.accountKey;
      }
    }
    return json({ accounts, defaultAccountKey, logins });
  }
  if (url.pathname === "/api/agents/account") {
    const agent = agents.find((a) => a.id === body.id);
    agent.accountKey = body.account_key;
    return json(agent);
  }
  if (url.pathname === "/api/limits") {
    limitReads++;
    const key = url.searchParams.get("account_key") || "default";
    if (key === "work" && delayWork) {
      delayed = () => json(limits(key));
      return;
    }
    return json(limits(wrongLimitsAccount && key === "work" ? "default" : key));
  }
  if (url.pathname === "/api/limits/reset") return json({ outcome: "reset" });
  if (url.pathname === "/api/costs") {
    const accountKey = url.searchParams.get("account_key") || "default";
    const todayUSD = { default: 5, work: 12, other: 23 }[accountKey];
    const reply = () =>
      json({ accountKey, data: { todayUSD, last30DaysUSD: todayUSD * 3 } });
    if (accountKey === "work" && delayCostWork) {
      delayedCosts = reply;
      return;
    }
    return reply();
  }
  if (url.pathname === "/api/leads") {
    const lead = makeLead(
      body.id,
      "Created conversation",
      body.account_key || defaultAccountKey,
      true,
    );
    agents.push(lead);
    return json(lead);
  }
  if (url.pathname === "/api/accounts/login") {
    const previous = logins.find((r) => r.requestId === body.request_id);
    if (previous) return json(previous);
    const id = `signed-in-${logins.length}`;
    accounts.push({ id, email: null, label: "New account", status: "pending" });
    const receipt = {
      requestId: body.request_id,
      accountKey: id,
      loginId: id,
      verificationUrl: "https://auth.openai.com/codex/device",
      userCode: "ABCD-1234",
      status: "pending",
    };
    logins.push(receipt);
    return json(receipt);
  }
  if (url.pathname === "/api/accounts/login/cancel") {
    const receipt = logins.find((r) => r.requestId === body.request_id);
    receipt.status = "cancelled";
    accounts.splice(
      accounts.findIndex((a) => a.id === receipt.accountKey),
      1,
    );
    return json(receipt);
  }
  if (url.pathname === "/api/transcript/stream") {
    res.writeHead(503);
    return res.end();
  }
  if (url.pathname === "/api/queue") {
    if (req.method === "POST" && body.action === "steer")
      claudeQueue = claudeQueue.filter((item) => item.id !== body.id);
    return json({
      items: claudeQueue,
      revision: `claude-queue-${claudeQueue.length}`,
      capabilities: {
        receipts: true,
        edit: true,
        cancel: true,
        reorder: true,
        steer: true,
      },
    });
  }
  if (url.pathname === "/api/agents/account-transfer") {
    const previous = transferReceipts.get(body.request_id);
    if (previous) {
      assert.deepEqual(body, previous);
      return json({ id: body.request_id, status: "completed" });
    }
    transferReceipts.set(body.request_id, body);
    const agent = agents.find((agent) => agent.id === body.id);
    agent.accountKey = body.account_key;
    agent.provider =
      accounts.find((account) => account.id === body.account_key)?.provider ||
      "codex";
    if (loseTransferResponse) {
      loseTransferResponse = false;
      res.statusCode = 503;
      return json({ error: "The transfer response was lost." });
    }
    return json({ id: body.request_id, status: "completed" });
  }
  if (url.pathname === "/api/claude/session") {
    if (body.action === "state") return json(claudeSession);
    if (body.action === "settings") {
      claudeSession.settings = body.settings;
      return json({ ok: true });
    }
    if (body.action === "commands")
      return json([
        { name: "context", description: "Show context use" },
        { name: "fixture-skill", description: "A native skill" },
      ]);
    if (body.action === "command") {
      if (failClaudeCommand) {
        failClaudeCommand = false;
        res.statusCode = 503;
        return json({ error: "The command response was lost." });
      }
      return json({ ok: true });
    }
    if (body.action === "rollback") {
      const boundary = claudeSession.turns.findIndex(
        (turn) => turn.id === body.turn_id,
      );
      if (boundary >= 0)
        claudeSession.turns = claudeSession.turns.slice(0, boundary);
      if (failClaudeRollback) {
        failClaudeRollback = false;
        claudeSession.controlOperation = {
          turnId: body.turn_id,
          requestId: body.request_id,
        };
        res.statusCode = 503;
        return json({ error: "Rollback receipt is not available yet." });
      }
      delete claudeSession.controlOperation;
      return json({ ok: true });
    }
    res.statusCode = 400;
    return json({ error: "Unsupported fixture Claude action" });
  }
  if (url.pathname === "/api/claude/profiles") {
    const account = body.account_key
      ? accounts.find((account) => account.id === body.account_key)
      : { id: "claude-added", provider: "claude", status: "ready" };
    Object.assign(account, { label: body.label, claudeOptions: body.options });
    if (!body.account_key) accounts.push(account);
    return json({ accounts, defaultAccountKey, logins });
  }
  if (url.pathname === "/api/voice/records")
    return json({ records: [], delivered: [], cursor: 0 });
  if (url.pathname.startsWith("/api/sync/")) {
    res.statusCode = 404;
    return json({ error: "Fixture uses HTTP snapshots" });
  }
  if (url.pathname.startsWith("/api/"))
    return json({ items: [], sessions: [] });
  try {
    const path = join(
      root,
      "web/dist",
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
let browser;
let debugPage;
try {
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
    args: ["--disable-extensions", "--no-first-run"],
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 900 },
  });
  debugPage = page;
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => {
    errors.push(error.message);
    console.error(error.stack);
  });
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  const picker = page.locator(".account-picker");
  const quota = page.getByRole("button", {
    name: "Account limits",
    exact: true,
  });
  const settings = page.getByRole("dialog", {
    name: "Chat settings",
    exact: true,
  });
  const openSettings = async () => {
    if (!(await settings.isVisible()))
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
    await picker.waitFor();
  };
  const closeSettings = async () => {
    if (!(await settings.isVisible())) return;
    await settings.getByRole("button", { name: "Close", exact: true }).click();
    await settings.waitFor({ state: "hidden" });
  };
  const expectAccount = async (email) => {
    await openSettings();
    await page.waitForFunction(
      (email) =>
        document.querySelector(".account-picker")?.textContent.includes(email),
      email,
    );
    await closeSettings();
  };
  const choose = async (email) => {
    await openSettings();
    await picker.click();
    await page.getByRole("menuitem").filter({ hasText: email }).click();
    await page.waitForFunction(
      (email) =>
        document.querySelector(".account-picker")?.textContent.includes(email),
      email,
    );
    await closeSettings();
  };
  await openSettings();
  await picker.click();
  await page.waitForFunction(() => {
    const labels = [...document.querySelectorAll(".account-weekly-limit")];
    return (
      labels.length === 3 &&
      labels.every((node) => node.textContent === "Weekly: 65% left")
    );
  });
  await page.screenshot({ path: join(evidence, "account-picker-weekly.png") });
  await page.keyboard.press("Escape");
  await closeSettings();
  const details = page.getByRole("region", {
    name: "Account limits details",
    exact: true,
  });
  const inspectLimits = async (check) => {
    const wasOpen = (await quota.getAttribute("aria-expanded")) === "true";
    if (!wasOpen) await quota.click();
    await details.waitFor();
    const result = await check(details);
    if (!wasOpen) {
      await quota.click();
      await details.waitFor({ state: "hidden" });
    }
    return result;
  };
  const limitText = () => inspectLimits((panel) => panel.innerText());
  const waitLimits = (text) =>
    inspectLimits((panel) =>
      panel.getByText(text, { exact: true }).first().waitFor(),
    );
  const waitCost = (text) =>
    inspectLimits(() =>
      page
        .getByRole("region", { name: "Local cost estimates" })
        .getByText(text, { exact: true })
        .waitFor(),
    );
  await expectAccount("personal@example.com");
  await quota.waitFor();
  wrongLimitsAccount = true;
  await choose("work@example.com");
  await waitLimits("Limits temporarily unavailable");
  assert.doesNotMatch(await limitText(), /89% left/);
  wrongLimitsAccount = false;
  await quota.click();
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await quota.click();
  await expectAccount("work@example.com");
  await waitLimits("78% left");
  assert.equal(
    bodies.find((r) => r.path === "/api/agents/account").body.account_key,
    "work",
  );

  // Late costs cannot replace the newly selected account's amount.
  await choose("personal@example.com");
  await waitCost("$5.00");
  delayCostWork = true;
  await choose("work@example.com");
  for (let n = 0; n < 100 && !delayedCosts; n++) await page.waitForTimeout(20);
  assert.ok(delayedCosts);
  await choose("another.long.account@example.com");
  await waitCost("$23.00");
  delayedCosts();
  delayCostWork = false;
  await page.waitForTimeout(100);
  assert.match(
    await inspectLimits(() =>
      page.getByRole("region", { name: "Local cost estimates" }).innerText(),
    ),
    /23.00/,
  );
  await choose("work@example.com");
  await waitCost("$12.00");

  // Each menu open reads all accounts; the Limits panel still reuses its cache.
  const readsBeforeSwitch = limitReads;
  await choose("personal@example.com");
  await choose("work@example.com");
  await page.waitForTimeout(100);
  assert.equal(limitReads, readsBeforeSwitch + accounts.length * 2);
  const forceRefresh = async () => {
    await quota.click();
    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await quota.click();
  };
  // A delayed response from account B must not replace account C's limits.
  await choose("personal@example.com");
  delayWork = true;
  await choose("work@example.com");
  await forceRefresh();
  await expectAccount("work@example.com");
  await choose("another.long.account@example.com");
  await waitLimits("67% left");
  assert.ok(delayed, "B request is pending");
  delayed();
  delayed = null;
  delayWork = false;
  await page.waitForTimeout(100);
  assert.match(await limitText(), /67% left/);
  const updated = limits("other");
  updated.at += 5;
  updated.data.rateLimits.primary.usedPercent = 41;
  snapshotLimits = { other: updated, default: limits("default") };
  await waitLimits("59% left");
  snapshotLimits = {};

  await quota.click();
  await page.getByRole("button", { name: "Apply reset", exact: true }).click();
  await page
    .getByRole("button", { name: "Use one reset credit", exact: true })
    .click();
  await page
    .getByText("Reset applied. Allowance updates from Codex.")
    .waitFor();
  const reset = bodies.find((r) => r.path === "/api/limits/reset").body;
  assert.equal(reset.account_key, "other");
  assert.equal(reset.account_id, "native-other");
  assert.equal(reset.credit_id, "credit-other");
  assert.match(
    await page
      .getByRole("region", { name: "Local cost estimates" })
      .innerText(),
    /23.00/,
  );
  await quota.click();

  // Switch actual conversations, including a pending read for another account.
  const quotaValues = () =>
    inspectLimits((panel) =>
      panel.locator(".account-limit-value").allTextContents(),
    );
  const otherQuota = await quotaValues();
  agents.push(
    makeLead("work-chat", "Work account conversation", "work", false),
  );
  await page.locator('[data-chat="work-chat"]').waitFor();
  await page.locator('[data-chat="started"]').click();
  await waitLimits("89% left");
  delayWork = true;
  await page.locator('[data-chat="work-chat"]').click();
  await expectAccount("work@example.com");
  assert.match(
    await limitText(),
    /78% left/,
    "return navigation uses the selected account cache while refresh waits",
  );
  await forceRefresh();
  for (let i = 0; i < 100 && !delayed; i++)
    await new Promise((resolve) => setTimeout(resolve, 20));
  assert.ok(delayed, "work account refresh remains pending");
  await page.locator('[data-chat="empty"]').click();
  await expectAccount("another.long.account@example.com");
  assert.deepEqual(await quotaValues(), otherQuota);
  delayed();
  delayed = null;
  delayWork = false;
  await page.waitForTimeout(100);
  assert.deepEqual(
    await quotaValues(),
    otherQuota,
    "late work response cannot change selected conversation limits",
  );

  await page.locator('[data-chat="started"]').click();
  await openSettings();
  await picker.click();
  const mutationsBeforeTransfer = bodies.filter((r) =>
    ["/api/agents/account", "/api/agents/account-transfer"].includes(r.path),
  ).length;
  await page
    .getByRole("menuitem")
    .filter({ hasText: "work@example.com" })
    .click();
  const transferDialog = page.getByRole("dialog", {
    name: "Transfer this chat",
    exact: true,
  });
  await transferDialog.waitFor();
  assert.equal(agents.find((a) => a.id === "started").accountKey, "default");
  assert.equal(
    bodies.filter((r) =>
      ["/api/agents/account", "/api/agents/account-transfer"].includes(r.path),
    ).length,
    mutationsBeforeTransfer,
  );
  await transferDialog
    .getByRole("button", { name: "Cancel", exact: true })
    .click();
  await picker.click();
  await page
    .getByRole("menuitem", { name: "Manage accounts · 3", exact: true })
    .click();
  const dialog = page.getByRole("dialog", {
    name: /^(Accounts|Add account)$/,
    exact: true,
  });
  await dialog.locator("[data-account]").first().waitFor();
  assert.equal(await dialog.locator("[data-account]").count(), 3);
  await dialog
    .locator('[data-account="work"]')
    .getByText("5h 78% left", { exact: true })
    .waitFor();
  assert.equal(await dialog.locator("time[datetime]").count(), 12);
  assert.equal(await dialog.getByText("Spark", { exact: true }).count(), 3);
  await dialog.getByText("5h Reset due", { exact: true }).waitFor();
  await dialog.getByText("7d Unknown", { exact: true }).waitFor();
  assert.match(
    await dialog.locator('[data-account="other"] time').nth(2).innerText(),
    /^Due .+\d/,
  );
  assert.equal(
    await dialog.getByText("7d 65% left", { exact: true }).count(),
    3,
  );
  assert.equal(
    await dialog.getByText("Edit rules", { exact: true }).count(),
    0,
  );
  const desktopBounds = await dialog.boundingBox();
  assert.ok(
    desktopBounds.y >= 0 && desktopBounds.y + desktopBounds.height <= 900,
    "Three accounts with both quota windows fit a 900px viewport",
  );
  await dialog
    .getByRole("button", {
      name: "Use work@example.com by default",
      exact: true,
    })
    .click();
  await dialog
    .locator('[data-account="work"]')
    .getByText("Application default", { exact: true })
    .waitFor();
  await dialog
    .getByRole("button", { name: "Find existing accounts", exact: true })
    .click();
  await page.waitForFunction(
    () => !document.querySelector(".accounts-actions button")?.disabled,
  );
  assert.equal(discoverCount, 1);
  assert.ok(
    await dialog.locator(".accounts-manager").evaluate((element) => {
      const top = element.getBoundingClientRect().top;
      const bottom = element.getBoundingClientRect().bottom;
      const dialog = element.closest('[role="dialog"]').getBoundingClientRect();
      return top >= dialog.top && bottom <= dialog.bottom;
    }),
    "All account manager content fits without scrolling at 900px",
  );
  await page.screenshot({
    path: join(evidence, "accounts-desktop.png"),
    animations: "disabled",
  });
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
  await closeSettings();
  await page.locator(".project-tree-heading").first().hover();
  await page
    .getByRole("button", { name: /^New chat in / })
    .first()
    .locator("..")
    .hover();
  await page
    .getByRole("button", { name: /^New chat in / })
    .first()
    .click();
  await page.waitForFunction(
    () =>
      document.querySelector("#conversation-title")?.textContent ===
      "Created conversation",
  );
  assert.equal(
    bodies.find((r) => r.path === "/api/leads").body.account_key,
    undefined,
  );

  await openSettings();
  await picker.click();
  await page
    .getByRole("menuitem", { name: "Manage accounts · 3", exact: true })
    .click();
  await dialog
    .getByRole("button", { name: "Add account", exact: true })
    .click();
  await dialog
    .getByRole("button", { name: "Sign in to another account", exact: true })
    .click();
  await dialog.getByText("ABCD-1234", { exact: true }).waitFor();
  assert.equal(
    await dialog
      .getByRole("link", { name: "Open sign-in page" })
      .getAttribute("href"),
    "https://auth.openai.com/codex/device",
  );
  const firstLogin = logins.at(-1).requestId;
  await page.reload();
  await openSettings();
  await picker.click();
  await page
    .getByRole("menuitem", { name: "Add account", exact: true })
    .click();
  await dialog.getByText("ABCD-1234", { exact: true }).waitFor();
  assert.equal(logins.length, 1, "reload resumes the existing native login");
  assert.equal(logins[0].requestId, firstLogin);
  await dialog
    .getByRole("button", { name: "Cancel sign-in", exact: true })
    .click();
  await dialog.getByText("Sign-in cancelled.", { exact: true }).waitFor();
  assert.equal(logins[0].status, "cancelled");
  await dialog
    .getByRole("button", { name: "Sign in to another account", exact: true })
    .click();
  await dialog.getByText("ABCD-1234", { exact: true }).waitFor();
  assert.equal(logins.length, 2);
  assert.notEqual(logins[1].requestId, firstLogin);
  accounts.find((a) => a.id === logins.at(-1).accountKey).status = "ready";
  await dialog.getByText("Account connected.", { exact: true }).waitFor();
  await dialog
    .getByRole("button", { name: "Back to accounts", exact: true })
    .click();
  await page.setViewportSize({ width: 820, height: 844 });
  await page.waitForFunction(
    () =>
      document.querySelector(".account-row") &&
      getComputedStyle(document.querySelector(".account-row")).display ===
        "grid",
  );
  await page.screenshot({
    path: join(evidence, "accounts-mobile.png"),
    animations: "disabled",
  });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
    "No page overflow on narrow screen",
  );
  const bounds = await dialog.boundingBox();
  assert.ok(
    bounds.x >= 0 && bounds.x + bounds.width <= 820,
    "Account manager fits narrow screen",
  );
  await page.setViewportSize({ width: 780, height: 720 });
  await page.screenshot({
    path: join(evidence, "accounts-780.png"),
    animations: "disabled",
  });
  assert.ok(
    await dialog.evaluate(
      (element) => element.scrollWidth <= element.clientWidth,
    ),
    "Account content does not overflow at 780px",
  );
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "hidden" });
  await page.screenshot({
    path: join(evidence, "account-header-narrow.png"),
    animations: "disabled",
  });
  await openSettings();
  assert.ok(await picker.isVisible());
  await closeSettings();
  // A stalled read must release the per-account request slot after its deadline.
  delayWork = true;
  delayed = null;
  await choose("work@example.com");
  await quota.click();
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  for (let i = 0; i < 100 && !delayed; i++) await page.waitForTimeout(20);
  assert.ok(delayed, "the timeout case has a pending work request");
  await page
    .locator(".account-limits-updated")
    .filter({ hasText: "Saved limits" })
    .waitFor({ timeout: 35000 });
  delayWork = false;
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await page
    .locator(".account-limits-updated")
    .filter({ hasText: "Saved limits" })
    .waitFor({ state: "hidden" });
  assert.match(await limitText(), /78% left/);
  assert.match(
    await inspectLimits(() =>
      page.getByRole("region", { name: "Local cost estimates" }).innerText(),
    ),
    /\$12.00/,
  );
  accounts.push({
    id: "claude",
    email: "claude@example.com",
    label: "Claude Code",
    provider: "claude",
    status: "ready",
    accountId: "native-claude",
  });
  const claude = {
    ...makeLead("claude-chat", "Claude conversation", "claude", false),
    provider: "claude",
    model: "default",
    inFlight: true,
    turnId: "claude-turn",
    status: "running",
  };
  agents.push(claude);
  claudeQueue = [
    {
      id: "claude-queued",
      agentId: claude.id,
      text: "Change the Claude task",
      status: "queued",
      created: Date.now() / 1000,
    },
  ];
  await page.reload();
  await page.locator(`[data-chat="${claude.id}"]`).click();
  await openSettings();
  await picker.click();
  const claudeOption = page
    .getByRole("menuitem")
    .filter({ hasText: "claude@example.com" });
  await claudeOption.getByText("Weekly: 96% left", { exact: true }).waitFor();
  await page.keyboard.press("Escape");
  await closeSettings();
  await quota.click();
  await details.waitFor();
  await details.getByText("89% left", { exact: true }).waitFor();
  await details.getByText("96% left", { exact: true }).waitFor();
  await details.getByText("93% left", { exact: true }).waitFor();
  assert.equal(
    (
      await details
        .locator(".account-limit-group > header > strong")
        .allTextContents()
    )[0],
    "Claude",
  );
  await page.screenshot({
    path: join(evidence, "claude-limits.png"),
    animations: "disabled",
  });
  await quota.click();
  await page
    .getByRole("button", { name: "Steer queued message 1", exact: true })
    .click();
  await page.waitForFunction(
    () => !document.querySelector('[aria-label="Steer queued message 1"]'),
  );
  const steer = bodies.find(
    (request) =>
      request.path === "/api/queue" && request.body.action === "steer",
  );
  assert.equal(steer?.body.id, "claude-queued");
  assert.equal(steer?.body.expectedText, "Change the Claude task");
  assert.ok(steer?.body.request_id);
  await openSettings();
  await picker.click();
  await page
    .getByRole("menuitem")
    .filter({ hasText: "Manage accounts" })
    .click();
  const manager = page.getByRole("dialog", { name: "Accounts", exact: true });
  const profile = manager.locator('[data-account="claude"]');
  await profile.getByText("Configure Claude", { exact: true }).click();
  assert.equal(
    await profile
      .getByLabel("Claude executable", { exact: true })
      .getAttribute("readonly"),
    "",
  );
  await profile
    .getByLabel("Custom models", { exact: true })
    .fill("custom-claude | Custom Claude");
  await profile
    .getByLabel("Automatic compaction threshold (tokens)", { exact: true })
    .fill("200000");
  await profile
    .getByRole("button", { name: "Save Claude profile", exact: true })
    .click();
  await profile.getByText("Claude profile saved.", { exact: true }).waitFor();
  const updatedProfile = bodies.find(
    (request) => request.path === "/api/claude/profiles",
  );
  assert.equal(updatedProfile.body.account_key, "claude");
  assert.equal(updatedProfile.body.options.autoCompactWindow, 200000);
  assert.deepEqual(updatedProfile.body.options.customModels, [
    { id: "custom-claude", label: "Custom Claude" },
  ]);
  await manager
    .getByRole("button", { name: "Add account", exact: true })
    .click();
  const addManager = page.getByRole("dialog", {
    name: "Add account",
    exact: true,
  });
  await addManager
    .getByText("Add a Claude Code profile", { exact: true })
    .click();
  await addManager
    .getByRole("textbox", { name: /^Profile name/ })
    .fill("Second Claude");
  await addManager
    .getByLabel("Claude configuration directory", { exact: true })
    .fill("/tmp/second-claude");
  await addManager
    .getByRole("button", { name: "Add Claude profile", exact: true })
    .click();
  await addManager
    .getByText("Claude profile saved.", { exact: true })
    .waitFor();
  const createdProfile = bodies
    .filter((request) => request.path === "/api/claude/profiles")
    .at(-1);
  assert.equal(createdProfile.body.account_key, undefined);
  assert.equal(createdProfile.body.options.configDir, "/tmp/second-claude");

  await addManager.getByRole("button", { name: "Close", exact: true }).click();
  await closeSettings();
  Object.assign(claude, { status: "completed", inFlight: false });
  await page.reload();
  await page.locator(`[data-chat="${claude.id}"]`).click();
  await openSettings();
  const claudeSettings = settings.getByRole("region", {
    name: "Claude settings",
    exact: true,
  });
  await claudeSettings
    .getByRole("heading", { name: "Claude Code 2.1.fixture", exact: true })
    .waitFor();
  await claudeSettings
    .getByLabel("Permission mode", { exact: true })
    .selectOption("plan");
  await claudeSettings
    .getByRole("switch", { name: "Extended thinking", exact: true })
    .uncheck();
  await claudeSettings
    .getByLabel("Auto-compact token limit", { exact: true })
    .fill("250000");
  await claudeSettings
    .getByRole("button", { name: "Save Claude settings", exact: true })
    .click();
  await page.waitForFunction(
    () =>
      !document.querySelector('[aria-label="Claude settings"] button')
        ?.disabled,
  );
  const sessionSettings = bodies
    .filter(
      (request) =>
        request.path === "/api/claude/session" &&
        request.body.action === "settings",
    )
    .at(-1);
  assert.equal(sessionSettings.body.id, claude.id);
  assert.deepEqual(sessionSettings.body.settings, {
    permissionMode: "plan",
    thinking: false,
    autoCompactWindow: 250000,
  });
  await claudeSettings
    .getByText("Commands and skills", { exact: true })
    .click();
  await claudeSettings
    .getByRole("button", { name: "Load commands", exact: true })
    .click();
  await claudeSettings
    .getByRole("option", { name: "/fixture-skill A native skill", exact: true })
    .waitFor({ state: "attached" });
  await claudeSettings
    .getByLabel("Command", { exact: true })
    .selectOption("/fixture-skill");
  assert.equal(
    await claudeSettings
      .getByLabel("Command and arguments", { exact: true })
      .inputValue(),
    "/fixture-skill",
  );
  await claudeSettings
    .getByLabel("Command and arguments", { exact: true })
    .fill("/fixture-skill check this");
  await claudeSettings
    .getByRole("button", { name: "Send command", exact: true })
    .click();
  await claudeSettings
    .getByRole("alert")
    .filter({ hasText: "The command response was lost." })
    .waitFor();
  await closeSettings();
  await page.reload();
  await page.locator(`[data-chat="${claude.id}"]`).click();
  await openSettings();
  await claudeSettings
    .getByRole("heading", { name: "Claude Code 2.1.fixture", exact: true })
    .waitFor();
  assert.equal(
    await claudeSettings
      .getByLabel("Permission mode", { exact: true })
      .inputValue(),
    "plan",
  );
  assert.equal(
    await claudeSettings
      .getByRole("switch", { name: "Extended thinking", exact: true })
      .isChecked(),
    false,
  );
  assert.equal(
    await claudeSettings
      .getByLabel("Auto-compact token limit", { exact: true })
      .inputValue(),
    "250000",
  );
  await claudeSettings
    .getByText("Commands and skills", { exact: true })
    .click();
  await claudeSettings
    .getByLabel("Command and arguments", { exact: true })
    .fill("/fixture-skill check this");
  await claudeSettings
    .getByRole("button", { name: "Send command", exact: true })
    .click();
  await claudeSettings
    .getByRole("button", { name: "Compact conversation", exact: true })
    .click();
  const commandsSent = bodies.filter(
    (request) =>
      request.path === "/api/claude/session" &&
      request.body.action === "command",
  );
  assert.deepEqual(
    commandsSent.map((request) => request.body.command),
    ["/fixture-skill check this", "/fixture-skill check this", "/compact"],
  );
  assert.deepEqual(commandsSent[1].body, commandsSent[0].body);
  for (const request of commandsSent) {
    assert.equal(request.body.id, claude.id);
    assert.match(request.body.request_id, /^[0-9a-f-]{36}$/);
  }
  assert.notEqual(
    commandsSent[0].body.request_id,
    commandsSent[2].body.request_id,
  );
  await claudeSettings
    .getByText("Conversation history", { exact: true })
    .click();
  await claudeSettings
    .getByLabel("First turn to remove", { exact: true })
    .selectOption("claude-turn-two");
  await claudeSettings
    .getByRole("button", { name: "Roll back context", exact: true })
    .click();
  await claudeSettings
    .getByRole("alert")
    .filter({ hasText: "Rollback receipt is not available yet." })
    .waitFor();
  await claudeSettings
    .getByRole("button", { name: "Roll back context", exact: true })
    .click();
  await claudeSettings
    .locator('option[value="claude-turn-two"]')
    .waitFor({ state: "detached" });
  const rollbacks = bodies.filter(
    (request) =>
      request.path === "/api/claude/session" &&
      request.body.action === "rollback",
  );
  assert.equal(rollbacks.length, 2);
  assert.equal(rollbacks[0].body.id, claude.id);
  assert.equal(rollbacks[0].body.turn_id, "claude-turn-two");
  assert.match(rollbacks[0].body.request_id, /^[0-9a-f-]{36}$/);
  assert.deepEqual(rollbacks[1].body, rollbacks[0].body);
  assert.equal(
    await claudeSettings
      .getByLabel("First turn to remove", { exact: true })
      .inputValue(),
    "",
  );
  assert.equal(
    await claudeSettings
      .getByRole("button", { name: "Roll back context", exact: true })
      .isEnabled(),
    false,
  );
  await page.screenshot({
    path: join(evidence, "claude-session-controls.png"),
    animations: "disabled",
  });

  await closeSettings();
  await page.locator('[data-chat="started"]').click();
  await openSettings();
  await picker.click();
  await page
    .getByRole("menuitem")
    .filter({ hasText: "claude@example.com" })
    .click();
  await transferDialog.waitFor();
  await transferDialog
    .getByText("The destination model uses the saved chat context.", {
      exact: true,
    })
    .waitFor();
  assert.match(
    await transferDialog.innerText(),
    /Subagents stay on their current accounts/,
  );
  await transferDialog
    .getByRole("button", { name: "Transfer chat", exact: true })
    .click();
  await transferDialog
    .getByRole("alert")
    .filter({ hasText: "The transfer response was lost." })
    .waitFor();
  await transferDialog
    .getByRole("button", { name: "Transfer chat", exact: true })
    .click();
  await transferDialog.waitFor({ state: "hidden" });
  const forwardTransfers = bodies.filter(
    (request) => request.path === "/api/agents/account-transfer",
  );
  assert.equal(forwardTransfers.length, 2);
  assert.deepEqual(forwardTransfers[0].body, forwardTransfers[1].body);
  assert.equal(forwardTransfers[0].body.id, "started");
  assert.equal(forwardTransfers[0].body.account_key, "claude");
  assert.match(forwardTransfers[0].body.request_id, /^[0-9a-f-]{36}$/);
  await closeSettings();
  await page.reload();
  await page.locator('[data-chat="started"]').click();
  await openSettings();
  await picker.click();
  await page
    .getByRole("menuitem")
    .filter({ hasText: "personal@example.com" })
    .click();
  await transferDialog.waitFor();
  await transferDialog
    .getByText("The destination model uses the saved chat context.", {
      exact: true,
    })
    .waitFor();
  await transferDialog
    .getByRole("button", { name: "Transfer chat", exact: true })
    .click();
  await transferDialog.waitFor({ state: "hidden" });
  const reverseTransfer = bodies
    .filter((request) => request.path === "/api/agents/account-transfer")
    .at(-1);
  assert.equal(reverseTransfer.body.id, "started");
  assert.equal(reverseTransfer.body.account_key, "default");
  assert.notEqual(
    reverseTransfer.body.request_id,
    forwardTransfers[0].body.request_id,
  );
  assert.equal(
    agents.find((agent) => agent.id === "started").provider,
    "codex",
  );

  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      cases: [
        "three identities",
        "empty chat account",
        "started chat account requires transfer confirmation",
        "delayed limits isolation",
        "wrong-account response rejection",
        "stalled read releases request slot",
        "fresh cache avoids reads on chat switch",
        "per-account snapshot limits",
        "chat switch with cached limits and delayed account response",
        "reset binding",
        "per-account costs",
        "default new chat",
        "discovery",
        "device login",
        "Codex and Spark dual-window quotas",
        "820px and 780px desktop layouts",
        "Claude native quotas and weekly picker",
        "Claude queue Steer request identity",
        "Claude profile create and update",
        "Claude session settings and native commands",
        "Claude rollback error and exact retry receipt",
        "Codex and Claude transfers with exact confirmation retry",
      ],
      evidence,
    }),
  );
} catch (error) {
  console.error(await debugPage?.locator("body").innerText());
  await debugPage?.screenshot({ path: join(evidence, "failure.png") });
  throw error;
} finally {
  if (delayed) delayed();
  await browser?.close();
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
}
