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
const evidence = await mkdtemp(join(tmpdir(), "codex-project-account-ui-"));
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
const projects = [
  {
    id: "/tmp/fixture",
    path: "/tmp/fixture",
    name: "fixture",
    created: 1,
    accountKey: "work",
    accountRevision: 1,
  },
  {
    id: "/projects/Lumina",
    path: "/projects/Lumina",
    name: "Lumina",
    created: 1,
    accountKey: "other",
    accountRevision: 1,
  },
];
let loseCreation = false;
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
          primary: { ...codex.primary, usedPercent: 6 },
          secondary: { ...codex.secondary, usedPercent: 15 },
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
        projects,
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
  if (url.pathname === "/api/projects") {
    let project = projects.find((item) => item.path === body.path);
    if (conflict) {
      res.statusCode = 409;
      return json({
        error: "Project account changed. Reopen to load the current account.",
      });
    }
    if (!project) {
      project = {
        id: body.path,
        path: body.path,
        name: body.path.split("/").at(-1),
        created: 1,
        accountRevision: 0,
      };
      projects.push(project);
    }
    assert.equal(body.expected_revision, project.accountRevision);
    project.accountKey = body.account_key;
    project.accountKeys = body.account_keys;
    project.accountRevision++;
    return json(project);
  }
  if (url.pathname === "/api/conversation") {
    const agent = agents.find((a) => a.id === body.id);
    agent.cwd = body.cwd;
    return json(agent);
  }
  if (url.pathname === "/api/agents/account") {
    const agent = agents.find((a) => a.id === body.id);
    agent.accountKey = body.account_key;
    return json(agent);
  }
  if (url.pathname === "/api/directories") {
    const path = url.searchParams.get("path") || "/projects";
    return json({ path, parent: "/projects", directories: [] });
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
    let lead = agents.find((item) => item.id === body.id);
    if (!lead) {
      lead = makeLead(
        body.id,
        "Created conversation",
        body.account_key ||
          projects.find((item) => item.path === body.cwd)?.accountKey ||
          defaultAccountKey,
        true,
      );
      lead.cwd = body.cwd;
      agents.push(lead);
    }
    if (loseCreation) {
      loseCreation = false;
      res.statusCode = 503;
      return json({ error: "Response lost" });
    }
    return json(lead);
  }
  if (url.pathname === "/api/voice/records")
    return json({ records: [], delivered: [], cursor: 0 });
  if (url.pathname.startsWith("/api/sync/")) {
    res.statusCode = 404;
    return json({ error: "Fixture uses HTTP snapshots" });
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
    viewport: { width: 1440, height: 900 },
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
  const settings = page.getByRole("dialog", {
    name: "Chat settings",
    exact: true,
  });
  const openSettings = async () => {
    if (!(await settings.isVisible()))
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
  };
  const closeSettings = async () => {
    if (await settings.isVisible()) {
      await settings.locator(".mantine-Modal-close").click();
      await settings.waitFor({ state: "hidden" });
    }
  };
  await openSettings();
  await picker.waitFor();
  await page.waitForFunction(() =>
    document
      .querySelector(".account-picker")
      ?.textContent.includes("personal@example.com"),
  );
  const choose = async (email) => {
    await openSettings();
    await picker.click();
    await page.getByRole("menuitem").filter({ hasText: email }).click();
  };
  await choose("work@example.com");
  await page.waitForFunction(() =>
    document
      .querySelector(".account-picker")
      ?.textContent.includes("work@example.com"),
  );
  assert.equal(agents.find((a) => a.id === "empty").cwd, "/tmp/fixture");
  assert.equal(
    await page
      .getByRole("dialog", { name: "Choose project folder", exact: true })
      .count(),
    0,
    "Account choice does not force folder choice",
  );
  await page
    .getByRole("button", { name: "Choose project folder", exact: true })
    .click();
  const folder = page.getByRole("dialog", {
    name: "Choose project folder",
    exact: true,
  });
  await folder.getByLabel("Folder path").fill("/anywhere/arbitrary");
  await folder.getByRole("button", { name: "Go", exact: true }).click();
  await folder
    .getByRole("button", { name: "Use this folder", exact: true })
    .click();
  await folder.waitFor({ state: "hidden" });
  assert.equal(agents.find((a) => a.id === "empty").cwd, "/anywhere/arbitrary");
  assert.equal(agents.find((a) => a.id === "empty").accountKey, "work");
  const newChat = async (name) => {
    await closeSettings();
    const button = page.getByRole("button", {
      name: `New chat in ${name}`,
      exact: true,
    });
    await button.locator("..").hover();
    await button.click();
  };
  const openProject = async (name) => {
    await closeSettings();
    const button = page.getByRole("button", {
      name: `Options for project ${name}`,
      exact: true,
    });
    await button.locator("..").hover();
    await button.click();
    await page
      .getByRole("menuitem", { name: "Project account", exact: true })
      .click();
  };
  const dialog = page.getByRole("dialog", {
    name: "Project account",
    exact: true,
  });
  await closeSettings();
  await page.locator('[data-chat="started"]').click();
  agents[0].status = "running";
  agents[0].inFlight = true;
  await openProject("fixture");
  const selection = dialog.getByLabel("Default account for new chats");
  assert.equal(await selection.inputValue(), "work");
  await dialog
    .getByRole("checkbox", {
      name: "another.long.account@example.com",
      exact: true,
    })
    .check();
  await selection.selectOption("other");
  conflict = true;
  await dialog
    .getByRole("button", { name: "Save accounts", exact: true })
    .click();
  await dialog.getByRole("alert").waitFor();
  assert.equal(
    await selection.inputValue(),
    "other",
    "Conflict keeps selected account",
  );
  assert.equal(projects[0].accountKey, "work");
  conflict = false;
  await dialog
    .getByRole("button", { name: "Save accounts", exact: true })
    .click();
  await dialog.waitFor({ state: "hidden" });
  assert.equal(projects[0].accountKey, "other");
  assert.deepEqual(
    new Set(projects[0].accountKeys),
    new Set(["work", "other"]),
  );
  assert.equal(
    agents[0].accountKey,
    "default",
    "Existing running session keeps its account",
  );
  const saves = bodies.filter((r) => r.path === "/api/projects");
  assert.deepEqual(
    saves[0].body,
    saves[1].body,
    "Retry keeps account and revision",
  );
  await openProject("arbitrary");
  await dialog
    .getByRole("checkbox", { name: "work@example.com", exact: true })
    .check();
  await selection.selectOption("work");
  await dialog
    .getByRole("button", { name: "Save accounts", exact: true })
    .click();
  await dialog.waitFor({ state: "hidden" });
  assert.equal(
    bodies.filter((r) => r.path === "/api/projects").at(-1).body
      .expected_revision,
    0,
    "Virtual project uses revision zero",
  );
  await newChat("fixture");
  await page.waitForFunction(
    () =>
      document.querySelector("#conversation-title")?.textContent ===
      "Created conversation",
  );
  assert.equal(agents.at(-1).accountKey, "other");
  assert.equal(
    bodies.filter((r) => r.path === "/api/leads").at(-1).body.account_key,
    "other",
  );
  await choose("personal@example.com");
  await newChat("arbitrary");
  await openSettings();
  await page.waitForFunction(() =>
    document
      .querySelector(".account-picker")
      ?.textContent.includes("work@example.com"),
  );
  assert.equal(
    agents.at(-1).accountKey,
    "work",
    "Project choice overrides previous empty chat account",
  );
  const before = agents.length;
  await newChat("arbitrary");
  await page.waitForFunction(
    (before) => document.querySelectorAll("[data-chat]").length > before,
    before,
  );
  assert.equal(
    agents.length,
    before + 1,
    "Explicit New chat creates a new chat",
  );
  loseCreation = true;
  await newChat("Lumina");
  await page
    .getByRole("button", { name: "Retry chat request", exact: true })
    .waitFor();
  projects[1].accountKey = "default";
  await page
    .getByRole("button", { name: "Retry chat request", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Retry chat request", exact: true })
    .waitFor({ state: "hidden" });
  const creates = bodies.filter((r) => r.path === "/api/leads");
  assert.deepEqual(
    creates.at(-1).body,
    creates.at(-2).body,
    "Lost response retains exact request identity",
  );
  assert.equal(
    agents.at(-1).accountKey,
    "other",
    "Committed request retains selected server account",
  );
  await openSettings();
  await picker.click();
  await page
    .getByRole("menuitem", { name: "Manage accounts · 3", exact: true })
    .click();
  const manager = page.getByRole("dialog", { name: "Accounts", exact: true });
  assert.equal(
    await manager.getByText("Edit rules", { exact: true }).count(),
    0,
  );
  assert.equal(
    await manager
      .getByRole("switch", { name: "Dangerously skip rules" })
      .count(),
    0,
  );
  await manager.locator(".mantine-Modal-close").click();
  await manager.waitFor({ state: "hidden" });
  await closeSettings();
  for (const width of [390, 320]) {
    await page.setViewportSize({ width, height: 844 });
    await page
      .getByRole("button", { name: "Toggle conversations", exact: true })
      .click();
    await openProject("fixture");
    assert.equal(
      await selection.inputValue(),
      width === 390 ? "other" : "work",
    );
    await page.waitForFunction(
      (element) => getComputedStyle(element).opacity === "1",
      await dialog.elementHandle(),
    );
    await page.screenshot({
      path: join(evidence, `project-account-${width}.png`),
      animations: "disabled",
    });
    const bounds = await dialog.boundingBox();
    assert.ok(bounds.x >= 0 && bounds.x + bounds.width <= width);
    assert.ok(
      await dialog.evaluate(
        (element) => element.scrollWidth <= element.clientWidth,
      ),
    );
    await dialog
      .getByRole("checkbox", {
        name:
          width === 390
            ? "work@example.com"
            : "another.long.account@example.com",
        exact: true,
      })
      .check();
    await selection.selectOption(width === 390 ? "work" : "other");
    await dialog
      .getByRole("button", { name: "Save accounts", exact: true })
      .click();
    await dialog.waitFor({ state: "hidden" });
  }
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      ok: true,
      cases: [
        "arbitrary account and folder",
        "project default",
        "running chat identity",
        "conflict retry",
        "virtual project",
        "new chats across projects",
        "lost response identity",
        "obsolete rules removed",
        "390px and 320px project settings",
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
