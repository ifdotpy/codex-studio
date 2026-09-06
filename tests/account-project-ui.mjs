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
const evidence = await mkdtemp(join(tmpdir(), "codex-account-rules-ui-"));
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
accounts[1].projectRules = {
  allowedProjects: ["/projects/Lumina"],
  revision: 1,
};
let defaultAccountKey = "default";
let conflict = false;
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
let delayed = null;
let delayWork = false;
let discoverCount = 0;
let snapshotLimits = {};
const limits = (key) => ({
  accountKey: key,
  at: Date.now() / 1000,
  data: {
    accountId: `native-${key}`,
    rateLimits: {
      limitId: "codex",
      planType: "pro",
      primary: {
        usedPercent: key === "default" ? 11 : key === "work" ? 22 : 33,
        windowDurationMins: 300,
        resetsAt: Date.now() / 1000 + 3600,
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
});
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
    return json({ accounts, defaultAccountKey });
  }
  if (url.pathname === "/api/accounts/rules") {
    const account = accounts.find((a) => a.id === body.account_key);
    if (
      conflict ||
      body.expected_revision !== (account.projectRules?.revision || 0)
    ) {
      res.statusCode = 409;
      return json({ error: "Project rules changed. Reload before saving." });
    }
    account.projectRules = {
      allowedProjects: body.allowed_projects,
      revision: body.expected_revision + 1,
    };
    return json({ accounts, defaultAccountKey });
  }
  if (url.pathname === "/api/conversation") {
    const agent = agents.find((a) => a.id === body.id);
    agent.dangerouslySkipAccountRules = body.dangerously_skip_rules;
    return json(agent);
  }
  if (url.pathname === "/api/agents/account") {
    if (body.account_key === "work") {
      res.statusCode = 403;
      return json({ error: "Project /tmp/fixture is not allowed for Work." });
    }
    const agent = agents.find((a) => a.id === body.id);
    agent.accountKey = body.account_key;
    return json(agent);
  }
  if (url.pathname === "/api/limits") {
    const key = url.searchParams.get("account_key") || "default";
    if (key === "work" && delayWork) {
      delayed = () => json(limits(key));
      return;
    }
    return json(limits(key));
  }
  if (url.pathname === "/api/limits/reset") return json({ outcome: "reset" });
  if (url.pathname === "/api/costs")
    return json({ data: { todayUSD: 12, last30DaysUSD: 40 } });
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
    accounts.push({
      id: "signed-in",
      email: null,
      label: "New account",
      status: "pending",
    });
    return json({
      accountKey: "signed-in",
      loginId: "login-fixture",
      verificationUrl: "https://auth.openai.com/codex/device",
      userCode: "ABCD-1234",
      status: "pending",
    });
  }
  if (url.pathname === "/api/transcript/stream") {
    res.writeHead(503);
    return res.end();
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
    viewport: { width: 1440, height: 950 },
  });
  debugPage = page;
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => {
    errors.push(error.message);
    console.error(error.message);
  });
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  const picker = page.locator(".account-picker");
  await picker.waitFor();
  await page.waitForFunction(() =>
    document
      .querySelector(".account-picker")
      ?.textContent.includes("personal@example.com"),
  );
  const manage = async () => {
    await picker.click();
    await page
      .getByRole("menuitem", { name: "Manage accounts · 3", exact: true })
      .click();
  };
  await manage();
  const dialog = page.getByRole("dialog", { name: "Accounts", exact: true });
  const work = dialog.locator('[data-account="work"]');
  await work.getByText("/projects/Lumina", { exact: true }).waitFor();
  await work
    .getByRole("button", { name: "Edit rules for work@example.com" })
    .click();
  await work
    .getByRole("textbox", { name: "Allowed project folders" })
    .fill("/projects/Lumina\n/projects/Lumina-tools");
  await work.getByRole("button", { name: "Save rules", exact: true }).click();
  await work.getByText("/projects/Lumina-tools", { exact: true }).waitFor();
  assert.deepEqual(accounts[1].projectRules, {
    allowedProjects: ["/projects/Lumina", "/projects/Lumina-tools"],
    revision: 2,
  });
  await page.keyboard.press("Escape");
  await page.reload();
  await picker.waitFor();
  await manage();
  await work.getByText("/projects/Lumina-tools", { exact: true }).waitFor();
  await work
    .getByRole("button", { name: "Edit rules for work@example.com" })
    .click();
  await work
    .getByRole("textbox", { name: "Allowed project folders" })
    .fill("/projects/draft");
  conflict = true;
  await work.getByRole("button", { name: "Save rules", exact: true }).click();
  await work
    .getByRole("alert")
    .filter({ hasText: "Project rules changed" })
    .waitFor();
  assert.equal(
    await work
      .getByRole("textbox", { name: "Allowed project folders" })
      .inputValue(),
    "/projects/draft",
  );
  assert.equal(accounts[1].projectRules.revision, 2);
  conflict = false;
  await work.getByRole("button", { name: "Cancel", exact: true }).click();
  await work
    .getByRole("button", { name: "Edit rules for work@example.com" })
    .click();
  await work.getByRole("textbox", { name: "Allowed project folders" }).fill("");
  await work.getByRole("button", { name: "Save rules", exact: true }).click();
  await work.getByText("No projects allowed", { exact: true }).waitFor();
  assert.deepEqual(accounts[1].projectRules.allowedProjects, []);
  await work
    .getByRole("button", { name: "Edit rules for work@example.com" })
    .click();
  await work.getByRole("radio", { name: "All projects", exact: true }).check();
  await work.getByRole("button", { name: "Save rules", exact: true }).click();
  await work
    .locator(".account-rules-summary")
    .getByText("All projects", { exact: true })
    .waitFor();
  assert.equal(accounts[1].projectRules.allowedProjects, null);
  await page.keyboard.press("Escape");
  await picker.click();
  await page
    .getByRole("menuitem")
    .filter({ hasText: "work@example.com" })
    .click();
  await picker.click();
  await page
    .getByRole("alert")
    .filter({ hasText: "Project /tmp/fixture is not allowed" })
    .waitFor();
  assert.ok(!agents.find((a) => a.id === "empty").dangerouslySkipAccountRules);
  const toggle = page.getByRole("switch", {
    name: "Dangerously skip rules",
    exact: true,
  });
  await toggle.click();
  await page.locator(".account-rules-badge").waitFor();
  assert.deepEqual(bodies.find((r) => r.path === "/api/conversation").body, {
    id: "empty",
    dangerously_skip_rules: true,
  });
  await page.reload();
  await page.locator(".account-rules-badge").waitFor();
  await manage();
  assert.equal(
    await dialog
      .getByRole("switch", { name: "Dangerously skip rules" })
      .isChecked(),
    true,
  );
  await page.screenshot({
    path: join(evidence, "rules-desktop.png"),
    animations: "disabled",
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await work
    .getByRole("button", { name: "Edit rules for work@example.com" })
    .click();
  await work
    .getByRole("radio", { name: "Only these projects", exact: true })
    .check();
  await work
    .getByRole("textbox", { name: "Allowed project folders" })
    .fill(
      "/projects/a-very-long-project-folder-name/another-really-long-nested-directory",
    );
  await page.screenshot({
    path: join(evidence, "rules-mobile.png"),
    animations: "disabled",
  });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
    "No page overflow",
  );
  const bounds = await dialog.boundingBox();
  assert.ok(bounds.x >= 0 && bounds.x + bounds.width <= 390);
  await dialog.getByRole("switch", { name: "Dangerously skip rules" }).click();
  await page.locator(".account-rules-badge").waitFor({ state: "hidden" });
  await page.keyboard.press("Escape");
  agents.push({
    ...makeLead("worker", "Busy worker", "default", false),
    rootId: "empty",
    isLead: false,
    inFlight: true,
    status: "running",
  });
  await page.reload();
  await picker.waitFor();
  await manage();
  assert.equal(
    await dialog
      .getByRole("switch", { name: "Dangerously skip rules" })
      .isDisabled(),
    true,
  );
  await dialog
    .getByText("Stop the team before you change this setting.")
    .waitFor();
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      cases: [
        "rule persistence",
        "revision conflict keeps draft",
        "empty list denies all",
        "explicit all projects",
        "denial visible",
        "explicit team override",
        "loaded override badge",
        "busy worker disables override",
        "390px layout",
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
