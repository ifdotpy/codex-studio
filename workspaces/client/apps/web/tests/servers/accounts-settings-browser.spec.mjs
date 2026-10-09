import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.mjs";
import { fixture } from "./multi-server-fixture.mjs";

test("accounts settings show per-server state and provider choices", async ({
  page,
  context,
}) => {
  test.setTimeout(120000);
  const localAccounts = [
    {
      id: "local-codex",
      label: "Work",
      provider: "codex",
      email: "person@example.test",
      plan: "Plus",
      status: "ready",
      isDefault: true,
    },
  ];
  const remoteAccounts = [
    {
      id: "remote-codex",
      label: "Work",
      provider: "codex",
      email: "person@example.test",
      plan: "Plus",
      status: "signedOut",
      disconnected: true,
    },
    {
      id: "remote-claude",
      label: "Claude personal",
      provider: "claude",
      email: "claude@example.test",
      plan: "Max",
      status: "ready",
      isDefault: true,
    },
  ];
  const local = await fixture("Local", false, localAccounts);
  const remote = await fixture("Remote", true, remoteAccounts);
  const empty = await fixture("Empty", true, []);
  await context.addInitScript(
    ({ destinations }) => {
      window.__studioAccountMessages = [];
      window.__studioMessages = [];
      window.addEventListener("message", (event) => {
        if (window === window.top) window.__studioMessages.push(event.data);
        if (event.data?.kind === "studio-server-accounts")
          window.__studioAccountMessages.push(event.data);
      });
      const nativeFetch = window.fetch.bind(window);
      window.fetch = async (input, init) => {
        const request = new Request(input, init);
        const url = new URL(request.url);
        const origin = destinations[url.origin];
        if (!origin) return nativeFetch(request);
        const body = await request.clone().arrayBuffer();
        return nativeFetch(origin + url.pathname + url.search, {
          method: request.method,
          headers: request.headers,
          ...(body.byteLength ? { body } : {}),
          signal: request.signal,
          redirect: "error",
          credentials: "omit",
        });
      };
    },
    {
      destinations: {
        [local.invitation.origin]: local.origin,
        [remote.invitation.origin]: remote.origin,
        [empty.invitation.origin]: empty.origin,
      },
    },
  );
  await page.goto(local.origin + "/?studio-navigation=combined");
  await page.locator("#message").waitFor();
  let pairedOneServer = false;
  const pair = async (server) => {
    const settingsButton = pairedOneServer
      ? page
          .getByRole("complementary", { name: "Servers and projects" })
          .getByRole("button", { name: "Studio settings", exact: true })
      : page.getByRole("button", { name: "Studio settings", exact: true });
    await settingsButton.click();
    const dialog = page.getByRole("dialog", { name: "Studio settings" });
    await dialog.getByRole("tab", { name: "Servers", exact: true }).click();
    await dialog
      .getByRole("button", { name: "Add with an invitation", exact: true })
      .click();
    await dialog.getByLabel("Server address").fill(server.invitation.origin);
    await dialog
      .getByLabel("Pairing invitation")
      .fill(JSON.stringify(server.invitation));
    await dialog
      .getByRole("button", { name: "Pair server", exact: true })
      .click();
    await expect(
      page.locator(`[data-settings-server="${server.invitation.serverId}"]`),
    ).toBeVisible();
    if (await dialog.isVisible().catch(() => false))
      await dialog.getByRole("button", { name: "Close", exact: true }).click();
    pairedOneServer = true;
  };
  await pair(remote);
  await pair(empty);
  await expect
    .poll(
      () =>
        page.evaluate(() =>
          window.__studioAccountMessages.some(
            (message) => message.accounts.length > 0,
          ),
        ),
      { timeout: 20000 },
    )
    .toBe(true);
  await page
    .getByRole("complementary", { name: "Servers and projects" })
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  const settings = page.getByRole("dialog", { name: "Studio settings" });
  await settings.getByRole("tab", { name: "Accounts", exact: true }).click();
  const accounts = settings.getByRole("region", {
    name: "Accounts",
    exact: true,
  });
  const work = accounts.locator(
    '[data-account-identity="codex:person@example.test"]',
  );
  await expect(work).toBeVisible();
  await expect(work.getByText("Default · Local")).toBeVisible();
  await expect(work.getByRole("button", { name: "✓ Local" })).toBeVisible();
  await expect(
    work.getByRole("button", { name: /sign in again/ }),
  ).toBeVisible();
  await expect(work.getByRole("button", { name: "+ Empty" })).toBeVisible();
  await expect(accounts.getByText("Claude personal")).toBeVisible();
  await expect(
    accounts
      .locator('[data-account-identity="claude:claude@example.test"]')
      .getByText("Default · Remote"),
  ).toBeVisible();

  const screenshots = fileURLToPath(
    new URL("../../../../../../docs/screenshots/", import.meta.url),
  );
  mkdirSync(screenshots, { recursive: true });
  await settings.screenshot({
    path: resolve(screenshots, "accounts-light.png"),
  });
  await page.emulateMedia({ colorScheme: "dark" });
  await settings.screenshot({
    path: resolve(screenshots, "accounts-dark.png"),
  });
  await page.setViewportSize({ width: 390, height: 844 });
  await settings.screenshot({
    path: resolve(screenshots, "accounts-mobile.png"),
  });

  await settings.getByRole("tab", { name: "Servers", exact: true }).click();
  const servers = settings.getByRole("region", {
    name: "Servers",
    exact: true,
  });
  await expect(servers.locator("[data-settings-server]")).toHaveCount(3);
  await page.setViewportSize({ width: 1280, height: 1100 });
  await page.mouse.move(0, 0);
  await page.emulateMedia({ colorScheme: "light" });
  await settings.screenshot({
    path: resolve(screenshots, "servers-cards-light.png"),
  });
  await page.emulateMedia({ colorScheme: "dark" });
  await settings.screenshot({
    path: resolve(screenshots, "servers-cards-dark.png"),
  });
  await page.emulateMedia({ colorScheme: "light" });
  await page.setViewportSize({ width: 390, height: 1500 });
  await page.mouse.move(0, 0);
  await settings.screenshot({
    path: resolve(screenshots, "servers-cards-mobile.png"),
  });
  await settings.getByRole("tab", { name: "Accounts", exact: true }).click();

  await accounts
    .getByRole("button", { name: "Add account", exact: true })
    .click();
  const add = page.getByRole("dialog", { name: "Add account" });
  await expect(add.getByLabel("Server")).toBeVisible();
  await expect(
    add.getByRole("button", { name: /Claude.*link \+ paste code/ }),
  ).toBeVisible();
  await add.getByRole("button", { name: /Claude.*link \+ paste code/ }).click();
  await add.getByLabel("Server").click();
  await page.getByRole("option", { name: "Remote" }).click();
  await add.getByLabel("Name").fill("New Claude");
  await add.getByLabel("Email (optional)").fill("new@example.test");
  await add.getByRole("button", { name: "Continue" }).click();
  await expect(
    page
      .frameLocator('iframe[title="Studio on Remote"]')
      .getByRole("dialog", { name: /Sign in to Claude · New Claude/ }),
  ).toBeVisible();
  await expect(settings).toBeHidden();
  const remoteFrame = page.frameLocator('iframe[title="Studio on Remote"]');
  const claudeSignIn = remoteFrame.getByRole("dialog", {
    name: /Sign in to Claude · New Claude/,
  });
  const addStarts = [];
  const addStartCounts = new Map();
  await page.route("**/api/accounts/claude/add", async (route) => {
    const request = route.request();
    if (request.method() !== "POST") return route.continue();
    const body = request.postDataJSON();
    const requestId = request.headers()["x-studio-request-id"];
    addStarts.push({ loginId: body.login_id, requestId });
    const prior = addStartCounts.get(body.login_id) || 0;
    addStartCounts.set(body.login_id, prior + 1);
    if (prior === 0 && addStartCounts.size <= 2) return route.abort();
    return route.continue();
  });
  await claudeSignIn.getByRole("button", { name: "Start sign-in" }).click();
  await expect(
    claudeSignIn.getByRole("button", { name: "Retry sign-in", exact: true }),
  ).toBeVisible();
  await claudeSignIn
    .getByRole("button", { name: "Retry sign-in", exact: true })
    .click();
  await expect(claudeSignIn.getByLabel("Paste code")).toBeVisible();
  expect(addStarts[1]?.loginId).toBe(addStarts[0]?.loginId);
  expect(addStarts[1]?.requestId).toBe(addStarts[0]?.requestId);
  await expect(
    claudeSignIn.getByRole("button", { name: "Copy link" }),
  ).toBeVisible();
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.emulateMedia({ colorScheme: "light" });
  await claudeSignIn.screenshot({
    path: resolve(screenshots, "accounts-claude-signin-light.png"),
  });
  await page.emulateMedia({ colorScheme: "dark" });
  await claudeSignIn.screenshot({
    path: resolve(screenshots, "accounts-claude-signin-dark.png"),
  });
  await page.emulateMedia({ colorScheme: "light" });
  await page.setViewportSize({ width: 390, height: 844 });
  await claudeSignIn.screenshot({
    path: resolve(screenshots, "accounts-claude-signin-mobile.png"),
  });
  await page.setViewportSize({ width: 1280, height: 1100 });
  const submittedCode = "FAKE-CLAUDE-CODE#FAKE-STATE";
  await claudeSignIn.getByLabel("Paste code").fill(submittedCode);
  await claudeSignIn.getByRole("button", { name: "Finish sign-in" }).click();
  await expect(claudeSignIn).toContainText("Signed in as new@example.test");
  expect(remote.accountWrites).toEqual(
    expect.arrayContaining([
      expect.objectContaining({
        path: "/api/accounts/claude/add",
        loginId: expect.any(String),
      }),
      expect.objectContaining({
        path: "/api/accounts/claude/login/code",
        loginId: expect.any(String),
        code: submittedCode,
      }),
    ]),
  );
  const claudeWrites = remote.accountWrites.filter((write) =>
    write.path.startsWith("/api/accounts/claude/"),
  );
  expect(claudeWrites).toHaveLength(2);
  expect(new Set(claudeWrites.map((write) => write.requestId)).size).toBe(2);
  expect(
    JSON.stringify(await page.evaluate(() => window.__studioMessages)),
  ).not.toContain(submittedCode);
  await claudeSignIn
    .getByRole("button", { name: "Close", exact: true })
    .last()
    .click();
  await expect(settings).toBeVisible();

  await accounts
    .getByRole("button", { name: "Add account", exact: true })
    .click();
  const secondClaudeAdd = page.getByRole("dialog", { name: "Add account" });
  await secondClaudeAdd
    .getByRole("button", { name: /Claude.*link \+ paste code/ })
    .click();
  await secondClaudeAdd.getByLabel("Server").click();
  await page.getByRole("option", { name: "Remote" }).click();
  await secondClaudeAdd.getByLabel("Name").fill("Second Claude");
  await secondClaudeAdd.getByRole("button", { name: "Continue" }).click();
  const secondClaudeSignIn = remoteFrame.getByRole("dialog", {
    name: /Sign in to Claude · Second Claude/,
  });
  await expect(
    secondClaudeSignIn.getByRole("button", {
      name: "Start sign-in",
      exact: true,
    }),
  ).toBeVisible();
  await secondClaudeSignIn
    .getByRole("button", { name: "Start sign-in", exact: true })
    .click();
  await expect(
    secondClaudeSignIn.getByRole("button", {
      name: "Retry sign-in",
      exact: true,
    }),
  ).toBeVisible();
  await expect(
    secondClaudeSignIn.getByRole("button", {
      name: "Start again",
      exact: true,
    }),
  ).toBeVisible({ timeout: 10000 });
  await secondClaudeSignIn
    .getByRole("button", { name: "Start again", exact: true })
    .click();
  await expect(
    secondClaudeSignIn.getByRole("link", { name: "Open sign-in page" }),
  ).toBeVisible();
  expect(addStarts[3]?.loginId).not.toBe(addStarts[2]?.loginId);
  expect(addStarts[3]?.requestId).not.toBe(addStarts[2]?.requestId);
  await secondClaudeSignIn
    .getByRole("button", { name: "Cancel", exact: true })
    .click();
  await expect(secondClaudeSignIn).toContainText("Sign-in cancelled.");
  await secondClaudeSignIn.getByLabel("Close", { exact: true }).click();
  await expect(settings).toBeVisible();

  await accounts
    .getByRole("button", { name: "Add account", exact: true })
    .click();
  const codexAdd = page.getByRole("dialog", { name: "Add account" });
  await codexAdd.getByRole("button", { name: /Codex.*one-time code/ }).click();
  await codexAdd.getByLabel("Server").click();
  await page.getByRole("option", { name: "Remote" }).click();
  await codexAdd.getByLabel("Name").fill("New Codex");
  await codexAdd.getByRole("button", { name: "Continue" }).click();
  const codexSignIn = remoteFrame.getByRole("dialog", {
    name: /Sign in to Codex · New Codex/,
  });
  await codexSignIn
    .getByRole("button", { name: "Sign in", exact: true })
    .click();
  await expect(
    codexSignIn.getByRole("button", { name: "Cancel sign-in" }),
  ).toBeVisible();
  await codexSignIn.screenshot({
    path: resolve(screenshots, "accounts-codex-signin-light.png"),
  });
  await codexSignIn.getByRole("button", { name: "Cancel sign-in" }).click();
  await expect(codexSignIn).toContainText("Sign-in cancelled.");
  const codexWrites = remote.accountWrites.filter((write) =>
    ["/api/accounts/login", "/api/accounts/login/cancel"].includes(write.path),
  );
  expect(codexWrites).toHaveLength(2);
  expect(new Set(codexWrites.map((write) => write.requestId)).size).toBe(2);
  await codexSignIn
    .getByRole("button", { name: "Close", exact: true })
    .last()
    .click();
  await expect(settings).toBeVisible();
  await work.getByRole("button", { name: /Remote · sign in again/ }).click();
  const reconnect = remoteFrame.getByRole("dialog", {
    name: /Sign in to Codex · person@example.test · Remote/,
  });
  await reconnect.getByRole("button", { name: "Start sign-in" }).click();
  await expect(reconnect).toContainText("Sign-in restored.");
  await expect
    .poll(
      () =>
        remote.accounts().find((account) => account.id === "remote-codex")
          ?.disconnected,
    )
    .toBe(false);
  await reconnect.getByLabel("Close").click();
  await expect(settings).toBeVisible();
});
