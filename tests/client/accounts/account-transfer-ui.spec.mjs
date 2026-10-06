#!/usr/bin/env node
import {
  test,
  expect,
  handleEntitySyncFixtureRequest,
  syncIdentityFixture,
} from "../playwright.mjs";
// Production React build with isolated account fixtures. No credentials or model calls.
import { createServer } from "node:http";
import { readFile, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, extname } from "node:path";
import { fileURLToPath } from "node:url";
test("Account Transfer Ui", async ({
  browser: _testBrowser,
  context: _testContext,
  page: testPage,
}) => {
  const assert = {
    equal: (actual, expected, message) =>
      expect(actual, message).toBe(expected),
    notEqual: (actual, expected, message) =>
      expect(actual, message).not.toBe(expected),
    deepEqual: (actual, expected, message) =>
      expect(actual, message).toEqual(expected),
    ok: (actual, message) => expect(actual, message).toBeTruthy(),
    match: (actual, expected, message) =>
      expect(actual, message).toMatch(expected),
    doesNotMatch: (actual, expected, message) =>
      expect(actual, message).not.toMatch(expected),
    fail: (message) => {
      throw new Error(message);
    },
  };

  const root = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
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
  let _delayed = null;
  let delayWork = false;
  let wrongLimitsAccount = false;
  let _discoverCount = 0;
  let _limitReads = 0;
  let delayCostWork = false;
  let _delayedCosts;
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
  const syncWorkspaceId = syncIdentityFixture().workspaceId;
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
    const stateForEntities = {
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
    };
    if (
      handleEntitySyncFixtureRequest(req, res, {
        snapshot: stateForEntities,
        workspaceId: syncWorkspaceId,
      })
    )
      return;
    if (url.pathname === "/api/session") return json({ token: "fixture" });

    if (
      url.pathname === "/api/accounts" ||
      url.pathname === "/api/accounts/discover" ||
      url.pathname === "/api/accounts/default"
    ) {
      if (url.pathname.endsWith("/default"))
        defaultAccountKey = body.account_key;
      if (url.pathname.endsWith("/discover")) _discoverCount++;
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
    if (url.pathname === "/api/agents/account-transfer") {
      const a = agents.find((a) => a.id === (body.id || "started"));
      if (body.action === "cancel") a.accountTransfer.status = "cancelled";
      else if (body.action === "retry")
        a.accountTransfer.needsAttention = false;
      else if (
        a.accountTransfer?.status === "pending" &&
        a.accountTransfer.targetAccountKey === body.account_key
      )
        a.accountTransfer.requestIds.push(body.request_id);
      else
        a.accountTransfer = {
          id: body.request_id,
          status: "pending",
          targetAccountKey: body.account_key,
          completed: 1,
          moved: 1,
          waitingCount: 5,
          total: 8,
          requestIds: [body.request_id],
          waiting: "Waiting for the current turn",
          interrupted: [
            {
              id: "worker-running",
              name: "Running worker",
              reason: "Moved to account Work",
            },
          ],
          blocked: [
            {
              id: "worker-blocked",
              name: "Blocked worker",
              reason: "Native receipt unavailable",
            },
          ],
          leftOnSource: [
            {
              id: "worker-claude",
              name: "Claude worker",
              provider: "claude",
              reason: "Uses claude; the destination account uses codex",
            },
          ],
        };
      return json(a.accountTransfer);
    }
    if (url.pathname === "/api/agents/account") {
      const agent = agents.find((a) => a.id === body.id);
      agent.accountKey = body.account_key;
      return json(agent);
    }
    if (url.pathname === "/api/limits") {
      _limitReads++;
      const key = url.searchParams.get("account_key") || "default";
      if (key === "work" && delayWork) {
        _delayed = () => json(limits(key));
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
        _delayedCosts = reply;
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
      accounts.push({
        id,
        email: null,
        label: "New account",
        status: "pending",
      });
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

    if (url.pathname.startsWith("/api/"))
      return json({ items: [], sessions: [] });
    try {
      const path = join(
        process.env.STUDIO_WEB_DIST || join(root, "web/dist"),
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
  try {
    const page = testPage;
    await page.setViewportSize({ width: 1280, height: 850 });
    page.setDefaultTimeout(10000);
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.locator('[data-chat="started"]').click();
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    const picker = page.locator(".account-picker");
    await picker.click();
    await page
      .getByRole("menuitem")
      .filter({ hasText: "work@example.com" })
      .click();
    const confirmation = page.getByRole("dialog", {
      name: "Transfer this chat",
      exact: true,
    });
    await confirmation.waitFor();
    assert.equal(
      bodies.filter((b) => b.path === "/api/agents/account-transfer").length,
      0,
      "account selection requires confirmation",
    );
    await confirmation
      .getByRole("button", { name: "Transfer chat", exact: true })
      .click();
    await confirmation.waitFor({ state: "hidden" });
    await page.waitForFunction(() =>
      document.querySelector(".account-picker")?.textContent.includes("1/8"),
    );
    assert.equal(
      bodies.filter((b) => b.path === "/api/agents/account-transfer").length,
      1,
    );
    assert.equal(
      bodies.find((b) => b.path === "/api/agents/account-transfer").body.id,
      "started",
    );
    assert.match(await picker.innerText(), /personal@example.com/);
    await picker.click();
    await page
      .getByText("Waiting for the current turn", { exact: true })
      .waitFor();
    await page
      .getByText("1 moved to work@example.com · 5 waiting", { exact: true })
      .waitFor();
    await page
      .getByText("Running worker: Moved to account Work", { exact: true })
      .waitFor();
    await page
      .getByText("Blocked worker blocked: Native receipt unavailable", {
        exact: true,
      })
      .waitFor();
    await page.getByText(/Claude worker left on source \(claude\)/).waitFor();
    const firstRequestId = bodies.find(
      (b) => b.path === "/api/agents/account-transfer",
    ).body.request_id;
    await page
      .getByRole("menuitem")
      .filter({ hasText: "work@example.com" })
      .click();
    const extension = page.getByRole("dialog", {
      name: "Add agents to this transfer",
      exact: true,
    });
    await extension.waitFor();
    await extension
      .getByRole("button", { name: "Add agents", exact: true })
      .click();
    await extension.waitFor({ state: "hidden" });
    const requestIds = bodies
      .filter((b) => b.path === "/api/agents/account-transfer")
      .map((b) => b.body.request_id);
    assert.equal(requestIds.length, 2);
    assert.equal(requestIds[0], firstRequestId);
    assert.notEqual(
      requestIds[0],
      requestIds[1],
      "a transfer extension gets its own stable request id",
    );
    await picker.click();
    assert.equal(
      await page
        .getByRole("menuitem")
        .filter({ hasText: "another.long.account@example.com" })
        .isDisabled(),
      true,
    );
    await page.waitForTimeout(180);
    await page.screenshot({
      path: join(evidence, "transfer-pending.png"),
      animations: "disabled",
    });
    await page
      .getByRole("button", { name: "Cancel remaining", exact: true })
      .click();
    await page.waitForFunction(
      () =>
        !document.querySelector(".account-picker")?.textContent.includes("1/8"),
    );
    assert.equal(agents[0].accountKey, "default");
    agents[0].accountTransfer = {
      id: "saved-request",
      status: "pending",
      targetAccountKey: "work",
      completed: 3,
      total: 8,
      needsAttention: true,
      canRetry: true,
      waiting: "The source account is offline",
    };
    await page.reload();
    await page.locator('[data-chat="started"]').click();
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    await picker.click();
    await page
      .getByRole("menu")
      .getByRole("button", { name: "Retry", exact: true })
      .click();
    assert.ok(
      bodies.some(
        (b) =>
          b.body.action === "retry" && b.body.request_id === "saved-request",
      ),
    );
    agents[0].accountKey = "work";
    agents[0].accountTransfer = {
      id: "lazy-history-request",
      status: "pending",
      targetAccountKey: "work",
      completed: 0,
      moved: 1,
      total: 1,
      nativeHistoryPending: 1,
      canFinishHistory: true,
    };
    await page.reload();
    await page.locator('[data-chat="started"]').click();
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    await picker.click();
    await page
      .getByText("History transfer in progress: 1 remaining.", { exact: true })
      .waitFor();
    assert.equal(
      await page
        .getByRole("button", { name: "Move history now", exact: true })
        .count(),
      0,
    );
    agents[0].accountTransfer = {
      ...agents[0].accountTransfer,
      status: "completed",
      completed: 1,
      nativeHistoryPending: 0,
    };
    await page.waitForFunction(
      () =>
        !document.querySelector(".account-picker")?.textContent.includes("0/1"),
    );
    assert.equal(
      bodies.filter((b) => b.body.action === "finish_history").length,
      0,
      "history needs no extra action",
    );
    assert.equal(agents[0].accountTransfer.status, "completed");
    agents[0].accountKey = "work";
    agents[0].accountTransfer = {
      ...agents[0].accountTransfer,
      status: "completed",
      completed: 8,
      moved: 7,
      waitingCount: 0,
      leftOnSource: [
        {
          id: "worker-claude",
          name: "Claude worker",
          provider: "claude",
          reason: "Uses claude; the destination account uses codex",
        },
      ],
    };
    await page.reload();
    await page.locator('[data-chat="started"]').click();
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    await page.waitForFunction(() =>
      document
        .querySelector(".account-picker")
        ?.textContent.includes("work@example.com"),
    );
    await picker.click();
    await page.getByText(/Claude worker left on source \(claude\)/).waitFor();
    await page.keyboard.press("Escape");
    await page.keyboard.press("Escape");
    await page.locator('[data-chat="empty"]').click();
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    await page.locator(".account-picker").click();
    await page
      .getByRole("menuitem")
      .filter({ hasText: "work@example.com" })
      .click();
    await page.waitForFunction(() =>
      document
        .querySelector(".account-picker")
        ?.textContent.includes("work@example.com"),
    );
    assert.ok(
      bodies.some(
        (b) => b.path === "/api/agents/account" && b.body.id === "empty",
      ),
    );
    assert.equal(
      defaultAccountKey,
      "default",
      "changing an empty lead does not change the application default",
    );
    await page.setViewportSize({ width: 390, height: 844 });
    await page.reload();
    assert.deepEqual(errors, []);
  } finally {
    server.closeAllConnections();
    server.close();
  }
});
