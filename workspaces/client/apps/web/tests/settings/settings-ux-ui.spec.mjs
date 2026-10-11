// Isolated account choices and disclosure flows. No real accounts or model calls.
import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { test, expect } from "../playwright.mjs";
import { spawnFixture as spawn } from "../playwright.mjs";

test("settings-ux-ui", async ({ page: fixturePage }) => {
  test.setTimeout(120_000);
  const root = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const evidence = await mkdtemp(join(tmpdir(), "studio-settings-ux-"));
  const proc = spawn(
    "python3",
    [
      "-B",
      join(root, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      evidence,
    ],
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
    await route.fulfill({
      json: {
        id: body.request_id,
        leadId: body.id,
        targetAccountKey: body.account_key,
        scope: body.scope,
        status: "pending",
        created: Date.now() / 1000,
        members: {},
      },
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
  await page.goto(`http://127.0.0.1:${port}`);
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
  const mainSettingsTrigger = page
    .locator(".composer-context-row")
    .getByRole("button", { name: "Main agent settings", exact: true });
  await mainSettingsTrigger.click();
  const executionSettings = page.locator(".execution-dropdown");
  await executionSettings.waitFor({ state: "visible" });
  const signedOutTile = executionSettings.getByRole("button", {
    name: "Select account Signed out",
    exact: true,
  });
  assert.equal(
    await signedOutTile.count(),
    0,
    "Signed-out accounts are not offered in the account tiles",
  );
  const workTile = executionSettings.getByRole("button", {
    name: "Select account Work",
    exact: true,
  });
  await workTile.click();
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
  await workTile.click();
  await transfer
    .getByRole("button", { name: "Transfer chat", exact: true })
    .click();
  await transfer.waitFor({ state: "hidden" });
  assert.equal(transferCalls, 1);
  await mainSettingsTrigger.click();
  await executionSettings.waitFor({ state: "visible" });
  await executionSettings
    .getByRole("group", { name: "Agent role", exact: true })
    .getByRole("button", { name: "Worker", exact: true })
    .click();
  const subagentAccount = executionSettings.getByRole("group", {
    name: "Subagent account",
    exact: true,
  });
  assert.equal(
    await executionSettings.getByRole("button", { name: /Save/ }).count(),
    0,
    "The account choice applies at once",
  );
  await subagentAccount
    .getByRole("button", { name: "Select account Work", exact: true })
    .click();
  await page.locator(".execution-error[role=alert]").waitFor();
  assert.equal(transferCalls, 2, "the account choice transfers without a save");
  assert.equal(
    await subagentAccount
      .getByRole("button", { name: "Automatic", exact: true })
      .getAttribute("aria-pressed"),
    "true",
    "a failed transfer reverts the account",
  );
  const subagentTransferResponse = page.waitForResponse((response) =>
    response.url().endsWith("/api/agents/account-transfer"),
  );
  await executionSettings
    .getByRole("button", { name: "Retry", exact: true })
    .click();
  await subagentTransferResponse;
  await executionSettings
    .getByRole("status")
    .filter({ hasText: /^Account transfer accepted\.$/ })
    .waitFor();
  assert.equal(transferCalls, 3);
  assert.ok(transferRequests[1].request_id);
  assert.equal(transferRequests[1].request_id, transferRequests[2].request_id);
  assert.equal(transferRequests[2].account_key, "work");
  assert.equal(transferRequests[2].scope, "subagents");
  // Pending transfers keep the stored tile selected; the acceptance status
  // does not name the target account until the transfer snapshot advances.
  assert.equal(
    await subagentAccount
      .getByRole("button", { name: "Automatic", exact: true })
      .getAttribute("aria-pressed"),
    "true",
    "the accepted transfer stays pending until replication updates the default",
  );
  await page.keyboard.press("Escape");
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth + 1,
    ),
  );
  await page.screenshot({ path: join(evidence, "accounts-390.png") });
  assert.ok(
    (
      await page
        .getByRole("button", { name: "Subagent defaults", exact: true })
        .textContent()
    )?.includes("Personal"),
    "Mobile settings retain the current subagent account name",
  );
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  const studioSettings = page.getByRole("dialog", {
    name: "Studio settings",
    exact: true,
  });
  assert.deepEqual(
    (await studioSettings.getByRole("tab").allTextContents()).map((label) =>
      label.trim(),
    ),
    ["Accounts", "Appearance", "Federation", "Servers", "Linux VM", "Hotkeys"],
    "Global Studio settings exposes each available settings tab",
  );
  await studioSettings
    .getByRole("tab", { name: "Accounts", exact: true })
    .click();
  await studioSettings
    .getByRole("tab", { name: "Appearance", exact: true })
    .click();
  await studioSettings
    .getByRole("radiogroup", { name: "Studio theme", exact: true })
    .getByRole("radio", { name: "Dark", exact: true })
    .check();
  await page.waitForFunction(
    () => document.documentElement.dataset.mantineColorScheme === "dark",
  );
  await studioSettings
    .getByRole("tab", { name: "Accounts", exact: true })
    .click();
  const transfersBeforeDefaultChange = transferCalls;
  const accountsPanel = studioSettings.getByRole("region", {
    name: "Accounts",
    exact: true,
  });
  await accountsPanel
    .getByRole("button", { name: "Add account", exact: true })
    .click();
  const add = page.getByRole("dialog", { name: "Add account", exact: true });
  await add.getByRole("group", { name: "Account type", exact: true }).waitFor();
  assert.equal(
    await add
      .getByRole("button", { name: "Continue", exact: true })
      .isDisabled(),
    true,
    "The account type must be chosen before starting sign-in",
  );
  await add.getByRole("button", { name: "Cancel", exact: true }).click();
  const workRow = accountsPanel
    .locator(".server-account-row")
    .filter({ hasText: "work@example.invalid" });
  await workRow.waitFor();
  await workRow
    .getByRole("button", { name: "Actions for work@example.invalid" })
    .click();
  await page
    .getByRole("menuitem", { name: "Make default", exact: true })
    .click();
  await workRow.getByText(/Default · /).waitFor();
  assert.equal(defaultAccountCalls, 1);
  assert.equal(defaultAccountKey, "work");
  await workRow
    .getByRole("button", { name: "Actions for work@example.invalid" })
    .click();
  await page.getByRole("menuitem", { name: "Sign out", exact: true }).click();
  const disconnect = page.getByRole("dialog", {
    name: "Disconnect account",
    exact: true,
  });
  await disconnect.waitFor();
  assert.equal(disconnectCalls, 0);
  await disconnect
    .getByRole("button", { name: "Disconnect account", exact: true })
    .click();
  const reconnect = workRow.locator(
    '.server-account-chip[data-state="reauth"]',
  );
  await reconnect.waitFor();
  await reconnect.click();
  const signIn = page.getByRole("dialog", {
    name: /Sign in to Codex · work@example\.invalid/,
  });
  await signIn.waitFor();
  await page.keyboard.press("Escape");
  assert.equal(disconnectCalls, 1);
  const personal = accountsPanel
    .locator(".server-account-row")
    .filter({ hasText: "personal@example.invalid" });
  await personal
    .getByRole("button", { name: "Actions for personal@example.invalid" })
    .click();
  await page.getByRole("menuitem", { name: "Remove", exact: true }).click();
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
  await reopenedSettings
    .getByRole("tab", { name: "Accounts", exact: true })
    .click();
  const reopenedAccounts = reopenedSettings.getByRole("region", {
    name: "Accounts",
    exact: true,
  });
  const reopenedPersonal = reopenedAccounts
    .locator(".server-account-row")
    .filter({ hasText: "personal@example.invalid" });
  await reopenedPersonal
    .getByRole("button", { name: "Actions for personal@example.invalid" })
    .click();
  await page.getByRole("menuitem", { name: "Remove", exact: true }).click();
  const reopenedDeletion = page.getByRole("dialog", {
    name: "Delete account",
    exact: true,
  });
  await reopenedDeletion.waitFor();
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
  await page.screenshot({
    path: join(evidence, "studio-settings-dark-390.png"),
  });
  await reopenedAccounts.screenshot({
    path: join(evidence, "accounts-dark-390.png"),
  });
  await page.keyboard.press("Escape");
  await reopenedSettings.waitFor({ state: "hidden" });
  assert.match(await mainSettingsTrigger.textContent(), /Work/);
  assert.equal(
    await page.locator("html").getAttribute("data-mantine-color-scheme"),
    "dark",
  );
  expect(errors).toEqual([]);
  console.log(
    "PASS context disclosure, truthful limits loading, account tile transfer confirmation, account rows, deletion retry, mobile layout. " +
      evidence,
  );
});
