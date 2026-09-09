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
    projectRules: {
      allowedProjects: ["/Users/igor/Projects/lumina"],
      revision: 1,
    },
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
const limits = (key) => {
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
      body.account_key,
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
  const choose = async (email) => {
    await picker.click();
    await page.getByRole("menuitem").filter({ hasText: email }).click();
  };
  await page.waitForFunction(() =>
    document
      .querySelector(".account-picker")
      ?.textContent.includes("personal@example.com"),
  );
  await quota.waitFor();
  wrongLimitsAccount = true;
  await choose("work@example.com");
  await page.waitForFunction(() =>
    document
      .querySelector(".account-limits-summary .account-limits-warning")
      ?.textContent.includes("Unavailable"),
  );
  assert.doesNotMatch(await quota.innerText(), /89% left/);
  wrongLimitsAccount = false;
  await quota.click();
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await quota.click();
  await page.waitForFunction(() =>
    document
      .querySelector(".account-picker")
      ?.textContent.includes("work@example.com"),
  );
  await page.waitForFunction(() =>
    document
      .querySelector(".account-limits-summary")
      ?.textContent.includes("78% left"),
  );
  assert.equal(
    bodies.find((r) => r.path === "/api/agents/account").body.account_key,
    "work",
  );

  // Late costs cannot replace the newly selected account's amount.
  await choose("personal@example.com");
  await page.waitForFunction(() =>
    document
      .querySelector(".account-cost-summary")
      ?.textContent.includes("$5.00"),
  );
  delayCostWork = true;
  await choose("work@example.com");
  for (let n = 0; n < 100 && !delayedCosts; n++) await page.waitForTimeout(20);
  assert.ok(delayedCosts);
  await choose("another.long.account@example.com");
  await page.waitForFunction(() =>
    document
      .querySelector(".account-cost-summary")
      ?.textContent.includes("$23.00"),
  );
  delayedCosts();
  delayCostWork = false;
  await page.waitForTimeout(100);
  assert.match(
    await page.locator(".account-cost-summary").innerText(),
    /23.00/,
  );
  await choose("work@example.com");
  await page.waitForFunction(() =>
    document
      .querySelector(".account-cost-summary")
      ?.textContent.includes("$12.00"),
  );

  // Fresh account cache must avoid another GET on return navigation.
  const readsBeforeSwitch = limitReads;
  await choose("personal@example.com");
  await choose("work@example.com");
  await page.waitForTimeout(100);
  assert.equal(limitReads, readsBeforeSwitch);
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
  await page.waitForFunction(() =>
    document
      .querySelector(".account-picker")
      ?.textContent.includes("work@example.com"),
  );
  await choose("another.long.account@example.com");
  await page.waitForFunction(() =>
    document
      .querySelector(".account-limits-summary")
      ?.textContent.includes("67% left"),
  );
  assert.ok(delayed, "B request is pending");
  delayed();
  delayed = null;
  delayWork = false;
  await page.waitForTimeout(100);
  assert.match(await quota.innerText(), /67% left/);
  const updated = limits("other");
  updated.at += 5;
  updated.data.rateLimits.primary.usedPercent = 41;
  snapshotLimits = { other: updated, default: limits("default") };
  await page.waitForFunction(() =>
    document
      .querySelector(".account-limits-summary")
      ?.textContent.includes("59% left"),
  );
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
    quota.locator(".account-limits-summary > span").nth(1).innerText();
  const otherQuota = await quotaValues();
  agents.push(
    makeLead("work-chat", "Work account conversation", "work", false),
  );
  await page.locator('[data-chat="work-chat"]').waitFor();
  await page.locator('[data-chat="started"]').click();
  await page.waitForFunction(() =>
    document
      .querySelector(".account-limits-summary")
      ?.textContent.includes("89% left"),
  );
  delayWork = true;
  await page.locator('[data-chat="work-chat"]').click();
  await page.waitForFunction(() =>
    document
      .querySelector(".account-picker")
      ?.textContent.includes("work@example.com"),
  );
  assert.match(
    await quota.innerText(),
    /78% left/,
    "return navigation uses the selected account cache while refresh waits",
  );
  await forceRefresh();
  for (let i = 0; i < 100 && !delayed; i++)
    await new Promise((resolve) => setTimeout(resolve, 20));
  assert.ok(delayed, "work account refresh remains pending");
  await page.locator('[data-chat="empty"]').click();
  await page.waitForFunction(() =>
    document
      .querySelector(".account-picker")
      ?.textContent.includes("another.long.account@example.com"),
  );
  assert.equal(await quotaValues(), otherQuota);
  delayed();
  delayed = null;
  delayWork = false;
  await page.waitForTimeout(100);
  assert.equal(
    await quotaValues(),
    otherQuota,
    "late work response cannot change selected conversation limits",
  );

  await page.locator('[data-chat="started"]').click();
  await picker.click();
  assert.equal(
    await page
      .getByRole("menuitem")
      .filter({ hasText: "work@example.com" })
      .isDisabled(),
    true,
  );
  await page
    .getByRole("menuitem", { name: "Manage accounts · 3", exact: true })
    .click();
  const dialog = page.getByRole("dialog", { name: "Accounts", exact: true });
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
  await dialog
    .getByText("/Users/igor/Projects/lumina", { exact: true })
    .waitFor();
  const desktopBounds = await dialog.boundingBox();
  assert.ok(
    desktopBounds.y >= 0 && desktopBounds.y + desktopBounds.height <= 900,
    "Three accounts with both quota windows and project rules fit a 900px viewport",
  );
  await dialog
    .getByRole("button", {
      name: "Use work@example.com by default",
      exact: true,
    })
    .click();
  await dialog
    .locator('[data-account="work"]')
    .getByText("Default", { exact: true })
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
  await page.locator(".project-tree-heading").first().hover();
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
    "work",
  );

  await picker.click();
  await page
    .getByRole("menuitem", { name: "Manage accounts · 3", exact: true })
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
  await page.locator('[data-testid="account-picker"]').click();
  await page.getByRole("menuitem", { name: /Manage accounts/ }).click();
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
  assert.ok(await picker.isVisible());
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
  assert.match(await quota.innerText(), /78% left/);
  assert.match(await quota.innerText(), /≈\$12.00 today/);
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      cases: [
        "three identities",
        "empty chat account",
        "pinned started chat",
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
