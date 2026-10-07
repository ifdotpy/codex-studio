// Production React build with isolated account fixtures. No credentials or model calls.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { access, readFile, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, extname } from "node:path";
import { fileURLToPath } from "node:url";

import { test, expect } from "../playwright.mjs";

async function runAccountsUi(mode, { page: fixturePage }) {
  const root = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const webDist = process.env.STUDIO_WEB_DIST || join(root, "web/dist");
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
  const archivedAccounts = [];
  let defaultAccountKey = "default";
  const logins = [];
  const claudeLogins = new Map();
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
  const limitReadAccounts = [];
  const accountMenuReadSets = [];
  let captureAccountMenuReads = false;
  let delayCostWork = false;
  let delayedCosts;
  let snapshotLimits = {};
  let claudeQueue = [];
  let stateReads = 0;
  let failClaudeSettings = false;
  const claudeSession = {
    version: "2.1.fixture",
    settings: { permissionMode: "default", thinking: true },
    turns:
      mode === "benchmark"
        ? Array.from({ length: 301 }, (_, i) => ({
            id: `claude-turn-${i}`,
            text: `Earlier request ${i}: ${"Review the change and report useful findings. ".repeat(18)}`,
            status: "completed",
          }))
        : [
            {
              id: "claude-turn-one",
              text: "Original request",
              status: "completed",
            },
            {
              id: "claude-turn-two",
              text: "Later request",
              status: "completed",
            },
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
    if (url.pathname === "/api/state") {
      stateReads++;
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
    }
    if (
      url.pathname === "/api/accounts" ||
      url.pathname === "/api/accounts/discover" ||
      url.pathname === "/api/accounts/default"
    ) {
      if (url.pathname.endsWith("/default"))
        defaultAccountKey = body.account_key;
      if (url.pathname.endsWith("/discover")) discoverCount++;
      for (const receipt of logins) {
        if (
          (!receipt.reauthAccountKey || receipt.nativeCompleted) &&
          accounts.find((a) => a.id === receipt.accountKey)?.status === "ready"
        ) {
          receipt.status = "ready";
          receipt.resolvedAccountKey = receipt.accountKey;
        }
      }
      return json({ accounts, archivedAccounts, defaultAccountKey, logins });
    }
    if (url.pathname === "/api/agents/account") {
      const agent = agents.find((a) => a.id === body.id);
      agent.accountKey = body.account_key;
      return json(agent);
    }
    if (url.pathname === "/api/limits") {
      const key = url.searchParams.get("account_key") || "default";
      limitReadAccounts.push(key);
      if (key === "work" && delayWork) {
        delayed = () => json(limits(key));
        return;
      }
      return json(
        limits(wrongLimitsAccount && key === "work" ? "default" : key),
      );
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
      const id = body.account_key || `signed-in-${logins.length}`;
      if (!body.account_key)
        accounts.push({
          id,
          email: null,
          label: "New account",
          status: "pending",
        });
      const receipt = {
        requestId: body.request_id,
        accountKey: id,
        ...(body.account_key ? { reauthAccountKey: id } : {}),
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
      if (!receipt.reauthAccountKey)
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
      if (req.method === "POST" && body.action === "cancel")
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
        if (failClaudeSettings) {
          failClaudeSettings = false;
          res.statusCode = 400;
          return json({ error: "The settings were rejected." });
        }
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
    if (url.pathname === "/api/accounts/claude/login") {
      if (req.method === "POST") {
        const receipt = {
          requestId: body.request_id,
          accountKey: body.account_key,
          status: "pending",
          verificationUrl: "https://claude.ai/oauth/authorize",
        };
        claudeLogins.set(receipt.requestId, receipt);
        return json(receipt);
      }
      return json(claudeLogins.get(url.searchParams.get("request_id")) || {});
    }
    if (url.pathname === "/api/claude/profiles") {
      const account = body.account_key
        ? accounts.find((account) => account.id === body.account_key)
        : { id: "claude-added", provider: "claude", status: "ready" };
      Object.assign(account, {
        label: body.label,
        claudeOptions: body.options,
      });
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
        webDist,
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
  let debugPage;
  try {
    const page = fixturePage;
    await fixturePage.setViewportSize({ width: 1440, height: 900 });
    debugPage = page;
    page.setDefaultTimeout(10000);
    const errors = [];
    page.on("pageerror", (error) => {
      errors.push(error.message);
      console.error(error.stack);
    });
    if (mode === "benchmark") {
      accounts.push({
        id: "claude",
        email: "claude@example.com",
        label: "Claude Code",
        provider: "claude",
        status: "ready",
        accountId: "native-claude",
      });
      const benchAgent = {
        ...makeLead("claude-chat", "Claude conversation", "claude", false),
        provider: "claude",
        model: "default",
      };
      for (let i = 0; i < 260; i++)
        agents.push(
          makeLead(`perf-${i}`, `Performance chat ${i}`, "default", false),
        );
      agents.push(benchAgent);
    }
    if (mode === "reauth") {
      accounts[0].accountId = "saved-default";
      accounts.push({
        id: "claude",
        email: "claude@example.com",
        label: "Claude",
        provider: "claude",
        status: "ready",
        accountId: "native-claude",
      });
      agents[0].accountKey = "claude";
      agents[0].provider = "claude";
      agents.push({
        ...makeLead("auth-worker", "Failed worker", "default", false),
        isLead: false,
        parentId: "started",
        rootId: "started",
        status: "failed",
        error: {
          message:
            "Your refresh token was revoked. Please log out and sign in again.",
          codexErrorInfo: "unauthorized",
        },
      });
    }
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    if (mode === "reauth") {
      await page
        .getByRole("button", { name: "Started conversation", exact: true })
        .click();
      await page
        .getByRole("button", { name: "Sign in to Codex", exact: true })
        .click();
      let login = page.getByRole("dialog", {
        name: "Sign in to personal@example.com",
        exact: true,
      });
      await page.route(
        "**/api/accounts/login",
        async (route) => {
          await route.fetch();
          await route.abort();
        },
        { times: 1 },
      );
      await login
        .getByRole("button", { name: "Start sign-in", exact: true })
        .click();
      await login.getByRole("alert").waitFor();
      const id = logins[0].requestId;
      assert.equal(
        logins[0].reauthAccountKey,
        "default",
        "A Claude lead must restore its Codex worker account",
      );
      await login
        .getByRole("button", { name: "Start sign-in", exact: true })
        .click();
      await login.getByText("ABCD-1234", { exact: true }).waitFor();
      assert.equal(
        logins.length,
        1,
        "A lost reply must reuse the same sign-in",
      );
      assert.equal(
        bodies.filter((r) => r.path === "/api/accounts/login").at(-1).body
          .request_id,
        id,
      );
      await page.reload();
      await page
        .getByRole("button", { name: "Sign in to Codex", exact: true })
        .click();
      login = page.getByRole("dialog", {
        name: "Sign in to personal@example.com",
        exact: true,
      });
      await login.getByText("ABCD-1234", { exact: true }).waitFor();
      assert.equal(logins.length, 1);
      assert.equal(
        await login
          .getByText("Sign-in restored. Send a new instruction to continue.")
          .count(),
        0,
        "Cached ready credentials do not confirm sign-in",
      );
      logins[0].nativeCompleted = true;
      await login
        .getByRole("button", { name: "Check status", exact: true })
        .click();
      await login
        .getByText("Sign-in restored. Send a new instruction to continue.", {
          exact: false,
        })
        .waitFor();
      assert.equal(accounts.filter((a) => a.id === "default").length, 1);
      assert.equal(
        agents.find((a) => a.id === "auth-worker").accountKey,
        "default",
      );
      assert.equal(
        bodies.filter((r) => /send|input|resume|turn/.test(r.path)).length,
        0,
        "Sign-in must not send a task",
      );
      await page.keyboard.press("Escape");
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
      await page
        .getByRole("dialog", { name: "Chat settings", exact: true })
        .locator(".account-picker")
        .first()
        .click();
      await page.getByRole("menuitem", { name: /Manage accounts/ }).click();
      await page
        .getByRole("dialog", { name: "Accounts", exact: true })
        .locator("[data-account=default]")
        .getByRole("button", { name: "Sign in again", exact: true })
        .click();
      await page
        .getByRole("dialog", {
          name: "Sign in to personal@example.com",
          exact: true,
        })
        .getByText("Sign-in restored. Send a new instruction to continue.", {
          exact: false,
        })
        .waitFor();
      assert.equal(logins.length, 1, "Accounts opens the same sign-in receipt");
      await page.setViewportSize({ width: 390, height: 844 });
      assert.ok(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth + 1,
        ),
      );
      await page.screenshot({
        path: join(evidence, "codex-sign-in-mobile.png"),
      });
      expect(errors).toEqual([]);
      console.log(
        "PASS Codex worker sign-in from Claude lead, exact retry, reload, native confirmation, no task replay, mobile",
      );
      console.log(`REAUTH evidence=${evidence}`);
      return;
    }

    if (mode === "benchmark") {
      const benchAgent = agents.find((agent) => agent.id === "claude-chat");
      const studioSettingsButton = page.locator("#studio-settings-toggle");
      await studioSettingsButton.waitFor();
      const studioSettings = page.getByRole("dialog", {
        name: "Studio settings",
        exact: true,
      });
      await page.setViewportSize({ width: 1440, height: 1000 });
      const openMs = await page.evaluate(
        () =>
          new Promise((resolve) => {
            const started = performance.now();
            const panel = () =>
              document.querySelector('[data-testid="studio-settings"]');
            const observer = new MutationObserver(() => {
              if (!panel()) return;
              observer.disconnect();
              resolve(performance.now() - started);
            });
            observer.observe(document.body, { childList: true, subtree: true });
            document.querySelector("#studio-settings-toggle").click();
            if (panel()) {
              observer.disconnect();
              resolve(performance.now() - started);
            }
          }),
      );
      await studioSettings.waitFor({ state: "visible" });
      const roundedOpenMs = Number(openMs.toFixed(1));
      const appearanceTab = studioSettings.getByRole("tab", {
        name: "Appearance",
        exact: true,
      });
      await appearanceTab.waitFor();
      await appearanceTab.click();
      await studioSettings
        .getByRole("region", { name: "Theme", exact: true })
        .waitFor();
      console.log(`PERF studio-settings-open-ms=${roundedOpenMs}`);
      await studioSettings.screenshot({
        path: join(evidence, "studio-settings-appearance-1440.png"),
        animations: "disabled",
      });
      await studioSettings
        .getByRole("button", { name: "Close", exact: true })
        .click();
      await studioSettings.waitFor({ state: "hidden" });
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
      const settings = page.getByRole("dialog", {
        name: "Chat settings",
        exact: true,
      });
      const claude = settings.getByRole("region", {
        name: "Claude settings",
        exact: true,
      });
      await claude.getByLabel("Permission mode", { exact: true }).waitFor();
      await page.waitForFunction(
        () =>
          document
            .querySelector('[aria-label="Claude settings"]')
            ?.getAttribute("aria-busy") === "false",
      );
      await page.screenshot({
        path: join(evidence, "chat-settings-1440.png"),
        animations: "disabled",
      });
      await page.setViewportSize({ width: 390, height: 844 });
      await page.screenshot({
        path: join(evidence, "chat-settings-390.png"),
        animations: "disabled",
      });
      await page.setViewportSize({ width: 320, height: 760 });
      assert.ok(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth + 1,
        ),
        "Chat settings must fit a 320px viewport",
      );
      assert.ok(
        await settings.evaluate(
          (element) => element.scrollWidth <= element.clientWidth + 1,
        ),
        "The settings dialog must not scroll horizontally at 320px",
      );
      await page.setViewportSize({ width: 1440, height: 1000 });
      const snapshotsBefore = stateReads;
      const savedAt = performance.now();
      await claude
        .getByLabel("Permission mode", { exact: true })
        .selectOption("plan");
      await claude.getByText("Saved", { exact: true }).waitFor();
      const saveMs = Number((performance.now() - savedAt).toFixed(1));
      assert.equal(
        stateReads,
        snapshotsBefore,
        "Setting changes must not refresh the full state snapshot",
      );
      const saveRequest = bodies
        .filter(
          (request) =>
            request.path === "/api/claude/session" &&
            request.body.action === "settings",
        )
        .at(-1);
      assert.equal(saveRequest.body.id, benchAgent.id);
      assert.match(saveRequest.body.request_id, /^[0-9a-f-]{36}$/);
      await page.screenshot({
        path: join(evidence, "chat-settings-saved-1440.png"),
        animations: "disabled",
      });
      await page.setViewportSize({ width: 390, height: 844 });
      await page.screenshot({
        path: join(evidence, "chat-settings-saved-390.png"),
        animations: "disabled",
      });
      await page.setViewportSize({ width: 1440, height: 1000 });
      const settingsSavedResponse = () =>
        page.waitForResponse(
          (response) =>
            response.url().endsWith("/api/claude/session") &&
            response.request().postDataJSON()?.action === "settings",
        );
      let saveResponse = settingsSavedResponse();
      await claude
        .getByRole("switch", { name: /^Extended thinking/ })
        .uncheck();
      assert.ok((await saveResponse).ok());
      await claude.getByText("Saved", { exact: true }).waitFor();
      await claude.getByText("Advanced", { exact: true }).click();
      saveResponse = settingsSavedResponse();
      await claude
        .getByLabel("Auto-compact token limit", { exact: true })
        .fill("250000");
      await settings.getByLabel("Permission mode", { exact: true }).focus();
      assert.ok((await saveResponse).ok());
      await claude.getByText("Saved", { exact: true }).waitFor();
      const finalSettings = bodies
        .filter(
          (request) =>
            request.path === "/api/claude/session" &&
            request.body.action === "settings",
        )
        .at(-1);
      assert.deepEqual(finalSettings.body.settings, {
        permissionMode: "plan",
        thinking: false,
        autoCompactWindow: 250000,
      });
      assert.equal(stateReads, snapshotsBefore);
      failClaudeSettings = true;
      await claude
        .getByLabel("Permission mode", { exact: true })
        .selectOption("default");
      await claude.getByRole("alert").waitFor();
      assert.equal(
        await claude
          .getByLabel("Permission mode", { exact: true })
          .inputValue(),
        "plan",
      );
      const failedSettingsRequest = bodies
        .filter(
          (request) =>
            request.path === "/api/claude/session" &&
            request.body.action === "settings",
        )
        .at(-1).body;
      const failedRequestId = failedSettingsRequest.request_id;
      const failedStorageKey = `claude-control:claude:${benchAgent.id}:settings:${JSON.stringify({ settings: failedSettingsRequest.settings })}`;
      assert.equal(
        await page.evaluate(
          (key) => JSON.parse(localStorage.getItem(key) || "null"),
          failedStorageKey,
        ),
        failedRequestId,
        "An unconfirmed setting keeps its exact request identity in localStorage",
      );
      await claude
        .getByLabel("Permission mode", { exact: true })
        .selectOption("default");
      await claude.getByText("Saved", { exact: true }).waitFor();
      const retriedRequestId = bodies
        .filter(
          (request) =>
            request.path === "/api/claude/session" &&
            request.body.action === "settings",
        )
        .at(-1).body.request_id;
      assert.equal(
        retriedRequestId,
        failedRequestId,
        "A rejected setting must retry with its saved request identity",
      );
      assert.ok(
        roundedOpenMs < 150,
        `Studio settings open was ${roundedOpenMs} ms, expected under 150 ms`,
      );
      assert.ok(
        saveMs < 300,
        `Setting save was ${saveMs} ms, expected under 300 ms`,
      );
      console.log(`PERF settings-change-to-saved-ms=${saveMs}`);
      console.log(
        `PERF state-refreshes-after-save=${stateReads - snapshotsBefore}`,
      );
      for (const name of [
        "chat-settings-390.png",
        "chat-settings-1440.png",
        "chat-settings-saved-390.png",
        "chat-settings-saved-1440.png",
      ]) {
        await access(join(evidence, name));
      }
      console.log(`PERF evidence=${evidence}`);
      return;
    }
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
      if (!(await settings.isVisible())) {
        const direct = page.getByRole("button", {
          name: "Chat settings",
          exact: true,
        });
        if (await direct.isVisible()) await direct.click();
        else {
          await page.getByRole("button", { name: "Chat actions" }).click();
          await page
            .getByRole("menuitem", { name: "Chat settings", exact: true })
            .click();
        }
      }
      await picker.waitFor();
    };
    const closeSettings = async () => {
      if (!(await settings.isVisible())) return;
      await settings
        .getByRole("button", { name: "Close", exact: true })
        .click();
      await settings.waitFor({ state: "hidden" });
    };
    const expectAccount = async (email) => {
      await openSettings();
      await page.waitForFunction(
        (email) =>
          document
            .querySelector(".account-picker")
            ?.textContent.includes(email),
        email,
      );
      await closeSettings();
    };
    const choose = async (email) => {
      await openSettings();
      const menuReadStart = limitReadAccounts.length;
      await picker.click();
      if (captureAccountMenuReads) {
        await page.waitForFunction(() => {
          const labels = [
            ...document.querySelectorAll(".account-weekly-limit"),
          ];
          return (
            labels.length === 3 &&
            labels.every((node) => !node.textContent.includes("loading"))
          );
        });
        accountMenuReadSets.push(limitReadAccounts.slice(menuReadStart));
      }
      await page.getByRole("menuitem").filter({ hasText: email }).click();
      await page.waitForFunction(
        (email) =>
          document
            .querySelector(".account-picker")
            ?.textContent.includes(email),
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
    await page.screenshot({
      path: join(evidence, "account-picker-weekly.png"),
    });
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
    for (let n = 0; n < 100 && !delayedCosts; n++)
      await page.waitForTimeout(20);
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
    captureAccountMenuReads = true;
    await choose("personal@example.com");
    await choose("work@example.com");
    captureAccountMenuReads = false;
    await page.waitForTimeout(100);
    assert.deepEqual(
      accountMenuReadSets,
      [
        accounts.map((account) => account.id),
        accounts.map((account) => account.id),
      ],
      "Opening each account menu reads each account once",
    );
    const forceRefresh = async () => {
      await quota.click();
      await page.getByRole("button", { name: "Refresh", exact: true }).click();
      await quota.click();
    };
    // A delayed response from account B must not replace account C's limits.
    await choose("personal@example.com");
    await choose("work@example.com");
    delayWork = true;
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
    await page
      .getByRole("button", { name: "Apply reset", exact: true })
      .click();
    await page
      .getByRole("button", { name: "Use one reset credit", exact: true })
      .click();
    await page.getByText("Reset applied.", { exact: true }).waitFor();
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
    agents.push(
      makeLead("work-chat", "Work account conversation", "work", false),
    );
    await page.locator('[data-chat="work-chat"]').waitFor();
    await page.locator('[data-chat="started"]').click();
    await waitLimits("89% left");
    await page.locator('[data-chat="work-chat"]').click();
    await expectAccount("work@example.com");
    assert.match(
      await limitText(),
      /78% left/,
      "return navigation uses the selected account cache while refresh waits",
    );
    delayWork = true;
    await forceRefresh();
    for (let i = 0; i < 100 && !delayed; i++)
      await new Promise((resolve) => setTimeout(resolve, 20));
    assert.ok(delayed, "work account refresh remains pending");
    await page.locator('[data-chat="empty"]').click();
    await expectAccount("another.long.account@example.com");
    const otherQuota = await quotaValues();
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
        ["/api/agents/account", "/api/agents/account-transfer"].includes(
          r.path,
        ),
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
        const dialog = element
          .closest('[role="dialog"]')
          .getBoundingClientRect();
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
    await dialog.getByRole("button", { name: "Sign in", exact: true }).click();
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
    await dialog.getByRole("button", { name: "Sign in", exact: true }).click();
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
    delayed = null;
    await choose("work@example.com");
    delayWork = true;
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
        requestedDelivery: "after_turn",
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
      .getByRole("button", { name: "Delete queued message 1", exact: true })
      .click();
    await page.waitForFunction(
      () => !document.querySelector('[aria-label="Delete queued message 1"]'),
    );
    const cancellation = bodies.find(
      (request) =>
        request.path === "/api/queue" && request.body.action === "cancel",
    );
    assert.equal(cancellation?.body.id, "claude-queued");
    assert.equal(cancellation?.body.expectedText, "Change the Claude task");
    assert.ok(cancellation?.body.request_id);
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
    await addManager.getByText("Claude", { exact: true }).click();
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

    await addManager
      .getByRole("button", { name: "Close", exact: true })
      .click();
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
      .getByLabel("Permission mode", { exact: true })
      .selectOption("plan");
    await claudeSettings.getByText("Saved", { exact: true }).waitFor();
    await claudeSettings
      .getByRole("switch", { name: /^Extended thinking/ })
      .uncheck();
    await claudeSettings.getByText("Saved", { exact: true }).waitFor();
    await claudeSettings.getByText("Advanced", { exact: true }).click();
    await claudeSettings
      .getByLabel("Auto-compact token limit", { exact: true })
      .fill("250000");
    await claudeSettings.locator(".claude-advanced > summary").click();
    await claudeSettings.getByText("Saved", { exact: true }).waitFor();
    await claudeSettings.getByText("Advanced", { exact: true }).click();
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
    assert.ok(
      bodies
        .filter(
          (request) =>
            request.path === "/api/claude/session" &&
            request.body.action === "settings",
        )
        .every((request) => /^[0-9a-f-]{36}$/.test(request.body.request_id)),
      "Each autosave retains an exact request identity",
    );
    await claudeSettings
      .getByText("Commands and skills", { exact: true })
      .click();
    await claudeSettings
      .getByRole("button", { name: "Load commands", exact: true })
      .click();
    await claudeSettings
      .getByRole("option", {
        name: "/fixture-skill A native skill",
        exact: true,
      })
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
    await claudeSettings.getByText("Advanced", { exact: true }).click();
    await claudeSettings
      .getByText("Claude Code 2.1.fixture", { exact: true })
      .waitFor();
    assert.equal(
      await claudeSettings
        .getByLabel("Permission mode", { exact: true })
        .inputValue(),
      "plan",
    );
    assert.equal(
      await claudeSettings
        .getByRole("switch", { name: /^Extended thinking/ })
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
      /Same provider subagents move too; other providers stay on their accounts/,
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

    const claudeIndex = accounts.findIndex(
      (account) => account.id === "claude",
    );
    const [archivedClaude] = accounts.splice(claudeIndex, 1);
    archivedAccounts.push({
      ...archivedClaude,
      status: "signedOut",
      deleted: true,
    });
    await page.reload();
    await page.locator(`[data-chat="${claude.id}"]`).click();
    await page
      .getByRole("button", { name: "Sign in to Claude", exact: true })
      .waitFor();
    await openSettings();
    await picker.click();
    assert.equal(
      await page
        .getByRole("menuitem")
        .filter({ hasText: "claude@example.com" })
        .count(),
      0,
      "deleted identities stay out of new account choices",
    );
    await picker.click();
    await closeSettings();
    await page
      .getByRole("button", { name: "Sign in to Claude", exact: true })
      .click();
    const archivedSignIn = page.getByRole("dialog", {
      name: "Sign in to Claude",
      exact: true,
    });
    await archivedSignIn
      .getByText("Use your Claude subscription for", { exact: false })
      .waitFor();
    await archivedSignIn
      .getByRole("button", { name: "Start sign-in", exact: true })
      .click();
    await archivedSignIn
      .getByRole("link", { name: "Open Claude sign-in", exact: true })
      .waitFor();
    assert.equal(
      [...claudeLogins.values()].at(-1).accountKey,
      "claude",
      "archived chat sign-in retains its native identity",
    );

    expect(errors).toEqual([]);
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
          "Claude queue cancel request identity",
          "Claude profile create and update",
          "Claude session settings and native commands",
          "Claude rollback error and exact retry receipt",
          "Codex and Claude transfers with exact confirmation retry",
          "archived Claude chat sign-in with active choices filtered",
        ],
        evidence,
      }),
    );
  } catch (error) {
    await debugPage
      ?.getByText("Error details", { exact: true })
      .click()
      .catch(() => {});
    console.error(await debugPage?.locator("body").innerText());
    await debugPage?.screenshot({ path: join(evidence, "failure.png") });
    throw error;
  } finally {
    if (delayed) delayed();
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
}

test("accounts-ui-smoke", async ({ page }) => {
  test.setTimeout(120_000);
  await runAccountsUi("default", { page });
});
test("accounts reauthentication recovery", async ({ page }) => {
  test.setTimeout(60_000);
  await runAccountsUi("reauth", { page });
});
test("accounts settings autosave @performance", async ({ page }) => {
  test.setTimeout(60_000);
  await runAccountsUi("benchmark", { page });
});
