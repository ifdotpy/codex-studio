#!/usr/bin/env node
// Isolated account choices and disclosure flows. No real accounts or model calls.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(root, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-settings-ux-"));
const proc = spawn(
  "python3",
  ["-B", join(root, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  log = "";
proc.stderr.on("data", (data) => {
  log += data;
});
try {
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
  const state = () => ({
    accounts,
    defaultAccountKey: "default",
    supportsDisconnect: true,
    logins: [],
  });
  let transferCalls = 0,
    disconnectCalls = 0,
    analyticsCalls = 0;
  let holdLimits = true;
  const held = [];
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1280, height: 900 },
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/accounts", (route) =>
    route.fulfill({ json: state() }),
  );
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
  await page.route("**/api/agents/account-transfer", async (route) => {
    transferCalls++;
    await route.fulfill({ json: { status: "pending" } });
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
    .getByText(
      "The context is the information the model can use in one response.",
    )
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
  const picker = page.getByTestId("account-picker");
  await picker.click();
  assert.ok(
    await page.getByRole("menuitem", { name: /Signed out/ }).isDisabled(),
  );
  await page.getByRole("menuitem", { name: /work@example.invalid/ }).click();
  const transfer = page.getByRole("dialog", {
    name: "Transfer this team",
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
  await picker.click();
  await page.getByRole("menuitem", { name: /work@example.invalid/ }).click();
  await transfer
    .getByRole("button", { name: "Transfer team", exact: true })
    .click();
  await transfer.waitFor({ state: "hidden" });
  assert.equal(transferCalls, 1);
  await picker.click();
  await page.getByRole("menuitem", { name: /Manage accounts/ }).click();
  const manager = page.getByRole("dialog", { name: "Accounts", exact: true });
  await manager.waitFor();
  assert.equal(
    await manager
      .getByRole("button", { name: "Sign in to another account" })
      .count(),
    0,
  );
  await manager
    .getByRole("button", { name: "Add account", exact: true })
    .click();
  const add = page.getByRole("dialog", { name: "Add account", exact: true });
  await add
    .getByRole("button", { name: "Sign in to another account" })
    .waitFor();
  assert.equal(await add.locator(".account-row").count(), 0);
  await add.getByRole("button", { name: "Back to accounts" }).click();
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
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth + 1,
    ),
  );
  await page.screenshot({ path: join(evidence, "accounts-390.png") });
  await work
    .getByRole("button", { name: "Reconnect account", exact: true })
    .click();
  await work
    .getByRole("button", { name: "Disconnect account", exact: true })
    .waitFor();
  await page.keyboard.press("Escape");
  await manager.waitFor({ state: "hidden" });
  const chatSettings = page.getByRole("dialog", {
    name: "Chat settings",
    exact: true,
  });
  assert.ok(
    await chatSettings.isVisible(),
    "Escape closes only account management",
  );
  assert.ok(
    await picker.locator(".account-picker-label").isVisible(),
    "Mobile settings retain the account name",
  );
  await chatSettings
    .getByLabel("Appearance", { exact: true })
    .selectOption("dark");
  await picker.click();
  await page.getByRole("menuitem", { name: /Manage accounts/ }).click();
  await manager.waitFor();
  await page.waitForFunction(
    (element) => getComputedStyle(element).opacity === "1",
    await manager.elementHandle(),
  );
  await page.screenshot({ path: join(evidence, "accounts-dark-390.png") });
  assert.equal(
    await page.locator("html").getAttribute("data-mantine-color-scheme"),
    "dark",
  );
  assert.deepEqual(errors, []);
  console.log(
    "PASS context disclosure, truthful limits loading, transfer confirmation, unavailable choices, add/manage separation, disconnect/reconnect, mobile layout. " +
      evidence,
  );
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
