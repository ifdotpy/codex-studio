// Isolated account choices and disclosure flows. No real accounts or model calls.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  accountTileButton,
  accountTileGroup,
  entityMutationFixtureResponse,
  expect,
  installApiSchemaFixtureHeaders,
  readFixtureSyncContract,
  readTestState,
  stubEntityState,
  test,
} from "../playwright.mjs";
import { spawnFixture as spawn } from "../playwright.mjs";

test("settings-ux-ui", async ({ page: fixturePage }) => {
  test.setTimeout(60_000);
  const root = dirname(
    dirname(dirname(dirname(fileURLToPath(import.meta.url)))),
  );
  const evidence = await mkdtemp(join(tmpdir(), "studio-settings-ux-"));
  const proc = spawn(
    process.env.PYTHON || "python3",
    ["-B", join(root, "tests/simple-ui-fixture.py"), evidence],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  let log = "";
  proc.stderr.on("data", (data) => {
    log += data;
  });
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const url = `http://127.0.0.1:${port}`;
  const snapshot = await readTestState(url);
  const syncContract = await readFixtureSyncContract(url);
  const accounts = [
    {
      id: "default",
      label: "Personal",
      email: "personal@example.invalid",
      status: "ready",
    },
    {
      id: "work",
      label: "Work",
      email: "work@example.invalid",
      status: "ready",
    },
    { id: "signedout", label: "Signed out", status: "signedOut" },
  ];
  const archivedAccounts = [];
  let defaultAccountKey = "default";
  const state = () => ({
    accounts,
    archivedAccounts,
    defaultAccountKey,
    supportsDisconnect: true,
    supportsDelete: true,
    logins: [],
  });
  const transferRequests = [];
  let transferCalls = 0,
    defaultAccountCalls = 0,
    disconnectCalls = 0,
    deleteCalls = 0,
    deleteRequestId = "",
    analyticsCalls = 0;
  let holdLimits = true;
  const held = [];
  const page = fixturePage;
  installApiSchemaFixtureHeaders(page);
  const entitySync = await stubEntityState(page, snapshot, syncContract);
  await fixturePage.setViewportSize({ width: 1280, height: 900 });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/accounts", (route) =>
    route.fulfill({ json: state() }),
  );
  await page.route("**/api/accounts/default", async (route) => {
    defaultAccountCalls++;
    defaultAccountKey = route.request().postDataJSON().account_key;
    await route.fulfill({ json: state() });
  });
  await page.route("**/api/models?*", (route) =>
    route.fulfill({
      json: {
        data: [
          {
            model: "gpt-6-luna",
            displayName: "Luna",
            provider: "codex",
            isDefault: true,
            supportedReasoningEfforts: [{ reasoningEffort: "high" }],
          },
        ],
      },
    }),
  );
  await page.route("**/api/conversation", async (route) => {
    const body = route.request().postDataJSON();
    await route.fulfill({
      json: {
        id: body.id,
        accountKey: "default",
        workerDefaults: body.worker_defaults,
      },
    });
  });
  await page.route("**/api/accounts/disconnect", async (route) => {
    disconnectCalls++;
    accounts.find(
      (account) => account.id === route.request().postDataJSON().account_key,
    ).disconnected = true;
    await route.fulfill({ json: state() });
  });
  await page.route("**/api/accounts/reconnect", async (route) => {
    delete accounts.find(
      (account) => account.id === route.request().postDataJSON().account_key,
    ).disconnected;
    await route.fulfill({ json: state() });
  });
  await page.route("**/api/accounts/delete", async (route) => {
    deleteCalls++;
    const requestId = route.request().postDataJSON().request_id;
    assert.ok(requestId, "deletion has a durable retry identity");
    if (deleteRequestId) assert.equal(requestId, deleteRequestId);
    deleteRequestId = requestId;
    if (deleteCalls === 1) {
      await route.fulfill({
        status: 500,
        json: { error: "Delete failed" },
      });
      return;
    }
    const key = route.request().postDataJSON().account_key;
    const [deleted] = accounts.splice(
      accounts.findIndex((account) => account.id === key),
      1,
    );
    archivedAccounts.push({ ...deleted, deleted: true });
    await route.fulfill({ json: state() });
  });
  await page.route("**/api/agents/account-transfer", async (route) => {
    transferCalls++;
    const body = route.request().postDataJSON();
    transferRequests.push(body);
    if (
      body.scope === "subagents" &&
      transferRequests.filter((request) => request.scope === "subagents")
        .length === 1
    )
      return route.abort();
    const agent = snapshot.runtime.agents.find((row) => row.id === body.id);
    assert.ok(agent, "the transfer target exists in the entity fixture");
    if (body.scope === "subagents")
      agent.workerDefaults = {
        ...agent.workerDefaults,
        accountKey: body.account_key,
      };
    else agent.accountKey = body.account_key;
    await entitySync.update(snapshot, { origin: url, token: snapshot.token });
    await route.fulfill({
      json: entityMutationFixtureResponse(snapshot, {
        status: "pending",
        targetAccountKey: body.account_key,
      }),
    });
  });
  const limits = (route) => ({
    accountKey:
      new URL(route.request().url()).searchParams.get("account_key") ||
      "default",
    at: Date.now() / 1000,
    data: {
      rateLimitsByLimitId: {
        codex: {
          limitId: "codex",
          primary: {
            usedPercent: 10,
            windowDurationMins: 300,
            resetsAt: Date.now() / 1000 + 3000,
          },
        },
      },
    },
  });
  await page.route("**/api/limits*", async (route) => {
    if (holdLimits) held.push(route);
    else await route.fulfill({ json: limits(route) });
  });
  await page.route("**/api/analytics?*", async (route) => {
    analyticsCalls++;
    await route.fulfill({ json: {} });
  });
  await page.goto(url);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.getByRole("button", { name: "Chat context", exact: true }).click();
  await page
    .getByText("80,000 of 200,000 tokens used (40%).", { exact: true })
    .waitFor();
  assert.equal(analyticsCalls, 0);
  assert.equal(await page.locator(".usage-footer .compactions").count(), 0);
  await page.keyboard.press("Escape");
  await page
    .getByRole("button", { name: "Account limits", exact: true })
    .click();
  await page.getByText("Loading account limits…", { exact: true }).waitFor();
  assert.equal(
    await page
      .getByText("Codex has not supplied account limits.", { exact: true })
      .count(),
    0,
  );
  holdLimits = false;
  for (const route of held) await route.fulfill({ json: limits(route) });
  await page
    .getByText("Loading account limits…", { exact: true })
    .waitFor({ state: "hidden" });
  await page.keyboard.press("Escape");
  await page
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  const workAccount = accountTileButton(page, "Account", "Work");
  assert.equal(
    await accountTileButton(page, "Account", "Personal").getAttribute(
      "aria-pressed",
    ),
    "true",
  );
  await workAccount.click();
  const transfer = page.getByRole("dialog", {
    name: "Transfer this chat",
    exact: true,
  });
  await transfer.waitFor();
  assert.equal(
    transferCalls,
    0,
    "Choosing an account must not transfer the team",
  );
  await transfer.getByRole("button", { name: "Cancel", exact: true }).click();
  assert.equal(transferCalls, 0);
  await workAccount.click();
  await transfer
    .getByRole("button", { name: "Transfer chat", exact: true })
    .click();
  await transfer.waitFor({ state: "hidden" });
  assert.equal(transferCalls, 1);
  await page
    .getByRole("dialog", { name: "Chat settings", exact: true })
    .getByRole("button", { name: "Main agent settings", exact: true })
    .click();
  await page
    .getByRole("dialog", { name: "Main agent settings", exact: true })
    .getByRole("group", { name: "Agent role", exact: true })
    .getByRole("button", { name: "Worker", exact: true })
    .click();
  const defaults = page.getByRole("dialog", {
    name: "Subagent defaults",
    exact: true,
  });
  const subagentAccount = accountTileGroup(page, "Subagent account");
  const originalSubagentAccount =
    await subagentAccount.getAttribute("data-value");
  await accountTileButton(page, "Subagent account", "Work").click();
  await defaults.locator(".execution-error[role=alert]").waitFor();
  assert.equal(transferCalls, 2, "the account choice transfers without a save");
  assert.equal(
    await subagentAccount.getAttribute("data-value"),
    originalSubagentAccount,
    "a failed transfer reverts the account",
  );
  const subagentTransferResponse = page.waitForResponse((response) =>
    response.url().endsWith("/api/agents/account-transfer"),
  );
  await defaults.getByRole("button", { name: "Retry", exact: true }).click();
  await subagentTransferResponse;
  await defaults
    .getByRole("status")
    .filter({ hasText: /^Saved/ })
    .waitFor();
  assert.equal(transferCalls, 3);
  assert.ok(transferRequests[1].request_id);
  assert.equal(transferRequests[1].request_id, transferRequests[2].request_id);
  assert.equal(transferRequests[2].account_key, "work");
  assert.equal(transferRequests[2].scope, "subagents");
  assert.equal(await subagentAccount.getAttribute("data-value"), "work");
  await page.keyboard.press("Escape");
  await page.keyboard.press("Escape");
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  const studioSettings = page.getByRole("dialog", {
    name: "Studio settings",
    exact: true,
  });
  const manager = studioSettings.locator(".accounts-manager-inline");
  await manager.waitFor();
  assert.equal(
    await manager.getByRole("button", { name: "Sign in" }).count(),
    0,
  );
  await manager
    .getByRole("button", { name: "Add account", exact: true })
    .click();
  await manager
    .getByRole("heading", { name: "Add account", exact: true })
    .waitFor();
  assert.equal(await manager.locator(".account-row").count(), 0);
  await manager.getByRole("button", { name: "Back to accounts" }).click();
  const work = manager.locator('[data-account="work"]');
  await work
    .getByRole("button", { name: "Disconnect account", exact: true })
    .click();
  const disconnect = page.getByRole("dialog", {
    name: "Disconnect account",
    exact: true,
  });
  await disconnect.waitFor();
  assert.equal(disconnectCalls, 0);
  await disconnect
    .getByRole("button", { name: "Disconnect account", exact: true })
    .click();
  await work
    .getByRole("button", { name: "Reconnect account", exact: true })
    .waitFor();
  assert.equal(disconnectCalls, 1);
  await work
    .getByRole("button", { name: "Reconnect account", exact: true })
    .click();
  await work
    .getByRole("button", { name: "Disconnect account", exact: true })
    .waitFor();
  await page.keyboard.press("Escape");
  await page
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  await page.setViewportSize({ width: 390, height: 844 });
  assert.equal(
    await accountTileButton(page, "Account", "Work").getAttribute(
      "aria-pressed",
    ),
    "true",
    "Mobile settings retain the selected account",
  );
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth + 1,
    ),
  );
  await page.keyboard.press("Escape");
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  assert.deepEqual(
    (await studioSettings.getByRole("tab").allTextContents()).map((label) =>
      label.trim(),
    ),
    ["Accounts", "Appearance", "Federation", "Linux VM", "Hotkeys"],
    "Global Studio settings exposes each available settings tab",
  );
  await studioSettings
    .getByRole("tab", { name: "Accounts", exact: true })
    .click();
  await studioSettings
    .getByRole("tab", { name: "Appearance", exact: true })
    .click();
  await studioSettings
    .getByLabel("Studio theme", { exact: true })
    .selectOption("dark");
  await page.waitForFunction(
    () => document.documentElement.dataset.mantineColorScheme === "dark",
  );
  await studioSettings
    .getByRole("tab", { name: "Accounts", exact: true })
    .click();
  const transfersBeforeDefaultChange = transferCalls;
  const studioManager = studioSettings.locator(".accounts-manager-inline");
  const workRow = studioManager.locator('[data-account="work"]');
  await workRow
    .getByRole("button", { name: "Use work@example.invalid by default" })
    .click();
  await workRow.getByText("Application default", { exact: true }).waitFor();
  assert.equal(defaultAccountCalls, 1);
  assert.equal(defaultAccountKey, "work");
  const personal = studioManager.locator('[data-account="default"]');
  await personal
    .getByRole("button", { name: "Delete account", exact: true })
    .click();
  const deletion = page.getByRole("dialog", {
    name: "Delete account",
    exact: true,
  });
  await deletion
    .getByRole("button", { name: "Delete account", exact: true })
    .click();
  await deletion.getByRole("alert").waitFor();
  assert.equal(deleteCalls, 1, "failed deletion remains visible and retryable");
  const [deleteStorageKey, deleteStorageValue] = await page.evaluate(() => {
    const key = Object.keys(localStorage).find((item) =>
      item.startsWith("codex-studio-account-delete-requests-v1:"),
    );
    return [key, key ? localStorage.getItem(key) : null];
  });
  assert.equal(
    deleteStorageKey,
    `codex-studio-account-delete-requests-v1:${evidence}`,
    "delete receipts are scoped to the current state directory",
  );
  assert.equal(JSON.parse(deleteStorageValue).default, deleteRequestId);
  await page.keyboard.press("Escape");
  await deletion.waitFor({ state: "hidden" });
  assert.ok(
    await studioSettings.isVisible(),
    "nested confirmation Escape keeps settings open",
  );
  await page.keyboard.press("Escape");
  await studioSettings.waitFor({ state: "hidden" });
  await page.reload();
  await page.locator("#message").waitFor();
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  const reopenedSettings = page.getByRole("dialog", {
    name: "Studio settings",
    exact: true,
  });
  const reopenedManager = reopenedSettings.locator(".accounts-manager-inline");
  const reopenedPersonal = reopenedManager.locator('[data-account="default"]');
  await reopenedPersonal
    .getByRole("button", { name: "Delete account", exact: true })
    .click();
  const reopenedDeletion = page.getByRole("dialog", {
    name: "Delete account",
    exact: true,
  });
  await reopenedDeletion
    .getByRole("button", { name: "Delete account", exact: true })
    .click();
  await reopenedPersonal.waitFor({ state: "detached" });
  assert.equal(deleteCalls, 2);
  await reopenedDeletion.waitFor({ state: "hidden" });
  assert.equal(
    transferCalls,
    transfersBeforeDefaultChange,
    "Changing the default account does not transfer the selected chat",
  );
  await page.keyboard.press("Escape");
  await reopenedSettings.waitFor({ state: "hidden" });
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  const finalSettings = page.getByRole("dialog", {
    name: "Studio settings",
    exact: true,
  });
  const finalManager = finalSettings.locator(".accounts-manager-inline");
  await finalManager
    .getByText("Application default", { exact: true })
    .waitFor();
  await page.waitForFunction(
    (element) => getComputedStyle(element).opacity === "1",
    await finalManager.elementHandle(),
  );
  assert.equal(
    await page.locator("html").getAttribute("data-mantine-color-scheme"),
    "dark",
  );
  expect(errors).toEqual([]);
  console.log(
    "PASS context disclosure, truthful limits loading, transfer confirmation, unavailable choices, add/manage separation, disconnect/reconnect, mobile layout. " +
      evidence,
  );
});
