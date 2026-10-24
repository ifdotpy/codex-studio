import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { test, expect } from "../playwright.mjs";
import { fixture } from "./multi-server-fixture.mjs";

function waitForPost(page, pathname, matches = () => true) {
  return page.waitForResponse((response) => {
    const request = response.request();
    if (
      new URL(response.url()).pathname !== pathname ||
      request.method() !== "POST"
    )
      return false;
    return matches(request.postDataJSON());
  });
}

test("accounts settings show per-server state and provider choices", async ({
  page,
  context,
}, testInfo) => {
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
    {
      id: "local-shared",
      label: "Shared",
      provider: "codex",
      email: "shared@example.test",
      plan: "Pro",
      status: "ready",
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
    {
      id: "remote-shared",
      label: "Shared",
      provider: "codex",
      email: "shared@example.test",
      plan: "Pro",
      status: "ready",
    },
  ];
  // Both servers read the same provider account; Remote has the newer reading.
  const sharedReadings = {
    "local-shared": { ageSeconds: 600, usedPercent: 10 },
    "remote-shared": { ageSeconds: 180, usedPercent: 67 },
  };
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
  await page.route("**/api/limits?*", async (route) => {
    const accountKey = new URL(route.request().url()).searchParams.get(
      "account_key",
    );
    const reading = sharedReadings[accountKey];
    const at = Date.now() / 1000 - (reading?.ageSeconds ?? 0);
    await route.fulfill({
      json: {
        accountKey,
        at,
        readAt: at,
        error: null,
        data: reading
          ? {
              rateLimitsByLimitId: {
                codex: {
                  limitId: "codex",
                  primary: {
                    usedPercent: reading.usedPercent,
                    windowDurationMins: 300,
                    resetsAt: at + 3000,
                  },
                },
              },
            }
          : null,
      },
    });
  });
  const pageClockStart = new Date();
  await page.clock.install({ time: pageClockStart });
  await page.goto(local.origin + "/?studio-navigation=combined");
  await page.locator("#message").waitFor();
  let pairedOneServer = false;
  const pair = async (server) => {
    const settingsButton = pairedOneServer
      ? page
          .getByRole("complementary", { name: "Conversations" })
          .getByRole("button", { name: "Studio settings", exact: true })
      : page.getByRole("button", { name: "Studio settings", exact: true });
    await settingsButton.click();
    const dialog = page.getByRole("dialog", { name: "Studio settings" });
    await dialog.getByRole("tab", { name: "Servers", exact: true }).click();
    await dialog
      .getByRole("button", { name: "Add with an invitation", exact: true })
      .click();
    await dialog
      .getByRole("textbox", { name: /^Server address/ })
      .fill(server.invitation.origin);
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
  await page
    .getByRole("complementary", { name: "Conversations" })
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
  await expect(work.getByText(/^Default.*Local$/)).toBeVisible();
  await work.getByRole("button", { name: /^Limit details for/ }).click();
  await expect(
    work.locator(".account-start-throttle > .disclosure-control"),
  ).toHaveText([
    /^Turn start limits on Local: \d+ per burst · \d+ s pause · \d+ running$/,
  ]);
  // A read that returns no data ends; it does not stay "Reading limits…".
  await expect(work.locator(".account-limit-summary")).toHaveText(
    /^Remaining limit\s*Unavailable$/,
  );
  // One provider account shows one remaining limit, not one per server.
  const shared = accounts.locator(
    '[data-account-identity="codex:shared@example.test"]',
  );
  await expect(shared.locator(".account-limit-summary")).toHaveCount(1);
  await expect(
    shared.getByRole("meter", { name: "Remaining limit" }),
  ).toHaveAttribute("aria-valuenow", "33");
  await expect(shared.locator(".account-limit-summary")).toContainText(
    "33% left",
  );
  await shared.getByRole("button", { name: /^Limit details for/ }).click();
  await expect(shared.locator(".account-limit-note")).toHaveText(
    /^Updated \d+ min ago on Remote$/,
  );
  await expect(
    shared.locator(".account-start-throttle > .disclosure-control"),
  ).toHaveText([
    /^Turn start limits on Local: /,
    /^Turn start limits on Remote: /,
  ]);
  await work.getByRole("button", { name: /^Limit details for/ }).click();
  await expect(work.getByRole("button", { name: "✓ Local" })).toBeVisible();
  await expect(
    work.getByRole("button", { name: /sign in again/ }),
  ).toBeVisible();
  await expect(work.getByRole("button", { name: "+ Empty" })).toBeVisible();
  await expect(accounts.getByText("Claude personal")).toBeVisible();
  await expect(
    accounts
      .locator('[data-account-identity="claude:claude@example.test"]')
      .getByText(/^Default.*Remote$/),
  ).toBeVisible();

  const profilePosts = [];
  await page.route("**/api/claude/profiles", async (route) => {
    const body = route.request().postDataJSON();
    profilePosts.push({
      port: new URL(route.request().url()).port,
      body,
    });
    remoteAccounts[1].claudeOptions = body.options;
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        accounts: remoteAccounts.map((account) => ({
          home: "/fixture/accounts/" + account.id,
          source: "fixture",
          ...account,
        })),
        defaultAccountKey: "remote-claude",
        archivedAccounts: [],
        logins: [],
      }),
    });
  });
  const remoteClaude = accounts.locator(
    '[data-account-identity="claude:claude@example.test"]',
  );
  await remoteClaude
    .getByRole("button", { name: "Actions for claude@example.test" })
    .click();
  await page.getByRole("menuitem", { name: "Configure", exact: true }).click();
  await expect(settings).toBeHidden();
  const profileFrame = page;
  const profile = profileFrame.getByRole("dialog", {
    name: "Configure Claude",
    exact: true,
  });
  await expect(profile).toBeVisible();
  await expect(page.locator(".server-views")).toBeVisible();
  await profile
    .getByLabel("Custom models", { exact: true })
    .fill("remote-model | Remote Model");
  await profile
    .getByLabel("Automatic compaction threshold (tokens)", { exact: true })
    .fill("225000");
  await profile
    .getByRole("button", { name: "Save Claude profile", exact: true })
    .click();
  await expect.poll(() => profilePosts.length).toBe(1);
  await expect(profile).toBeVisible();
  expect(profilePosts).toHaveLength(1);
  expect(profilePosts[0].port).toBe(new URL(remote.origin).port);
  expect(profilePosts[0].body.account_key).toBe("remote-claude");
  expect(profilePosts[0].body.options.autoCompactWindow).toBe(225000);
  expect(profilePosts[0].body.options.customModels).toEqual([
    { id: "remote-model", label: "Remote Model" },
  ]);
  await profile.getByRole("button", { name: "Close", exact: true }).click();
  await expect(profile).toBeHidden();
  await expect(settings).toBeVisible();
  await expect(remoteClaude).toBeVisible();
  const remoteActions = remoteClaude.getByRole("button", {
    name: "Actions for claude@example.test",
  });
  await expect(remoteActions).toBeFocused();
  await remoteActions.click();
  await page.getByRole("menuitem", { name: "Configure", exact: true }).click();
  await expect(settings).toBeHidden();
  const reopenedProfile = profileFrame.getByRole("dialog", {
    name: "Configure Claude",
    exact: true,
  });
  await expect(
    reopenedProfile.getByLabel("Custom models", { exact: true }),
  ).toHaveValue("remote-model | Remote Model");
  await expect(
    reopenedProfile.getByLabel("Automatic compaction threshold (tokens)", {
      exact: true,
    }),
  ).toHaveValue("225000");
  await reopenedProfile
    .getByRole("button", { name: "Close", exact: true })
    .click();
  await expect(settings).toBeVisible();
  await expect(remoteClaude).toBeVisible();
  await expect(remoteActions).toBeFocused();

  const updateDocumentationScreenshots =
    process.env.CODEX_UPDATE_DOC_SCREENSHOTS === "1";
  const screenshots = updateDocumentationScreenshots
    ? fileURLToPath(
        new URL("../../../../../../docs/screenshots/", import.meta.url),
      )
    : testInfo.outputPath("screenshots");
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
  if (
    await settings
      .getByRole("button", { name: "Back", exact: true })
      .isVisible()
  )
    await settings.getByRole("button", { name: "Back", exact: true }).click();
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
    page.getByRole("dialog", { name: /Sign in to Claude · New Claude/ }),
  ).toBeVisible();
  await expect(settings).toBeHidden();
  const remoteFrame = page;
  const claudeSignIn = remoteFrame.getByRole("dialog", {
    name: /Sign in to Claude · New Claude/,
  });
  const addStarts = [];
  const addStartCounts = new Map();
  let secondClaudeLoginId;
  let resolveSecondClaudeStartAbort;
  const secondClaudeStartAborted = new Promise((resolve) => {
    resolveSecondClaudeStartAbort = resolve;
  });
  await page.route("**/api/accounts/claude/add", async (route) => {
    const request = route.request();
    if (request.method() !== "POST") return route.continue();
    const body = request.postDataJSON();
    const requestId = request.headers()["x-studio-request-id"];
    addStarts.push({ loginId: body.login_id, requestId });
    const prior = addStartCounts.get(body.login_id) || 0;
    addStartCounts.set(body.login_id, prior + 1);
    if (prior === 0 && addStartCounts.size <= 2) {
      if (body.label === "Second Claude") {
        secondClaudeLoginId = body.login_id;
        resolveSecondClaudeStartAbort();
      }
      return route.abort();
    }
    return route.continue();
  });
  await claudeSignIn.getByRole("button", { name: "Start sign-in" }).click();
  await expect(
    claudeSignIn.getByRole("button", { name: "Retry sign-in", exact: true }),
  ).toBeVisible();
  await claudeSignIn
    .getByRole("button", { name: "Retry sign-in", exact: true })
    .click();
  await expect(
    claudeSignIn.getByLabel("Final authorization response"),
  ).toBeVisible();
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
  let codeWasSubmitted = false;
  let statusConfirmsCodeSubmission = false;
  let verificationReady = false;
  let finalStatusBarrier;
  const loginRequestId = addStarts[0]?.loginId;
  // The provider has not acknowledged code delivery yet. Keep the stream's
  // current receipt pending while the explicit recovery reads below are gated.
  remote.setClaudeProgress(loginRequestId, {
    requestId: loginRequestId,
    accountKey: loginRequestId,
    status: "pending",
    codeSubmitted: false,
  });
  const codeAttempts = [];
  await page.route("**/api/accounts/claude/login/code", async (route) => {
    const request = route.request();
    codeAttempts.push({
      body: request.postDataJSON(),
      requestId: request.headers()["x-studio-request-id"],
    });
    if (codeAttempts.length === 1) {
      return route.fulfill({
        status: 400,
        contentType: "application/json",
        body: JSON.stringify({
          error:
            "Paste the full authorization response from Claude after sign-in, with the code and state separated by #. Complete any email verification on Claude's sign-in page first.",
          outcome: "not_applied",
        }),
      });
    }
    await route.fetch();
    codeWasSubmitted = true;
    await route.abort();
  });
  await page.route("**/api/accounts/claude/login?*", async (route) => {
    const url = new URL(route.request().url());
    if (
      !codeWasSubmitted ||
      url.searchParams.get("request_id") !== loginRequestId
    )
      return route.continue();
    await finalStatusBarrier;
    await route.fulfill({
      json: {
        requestId: loginRequestId,
        accountKey: loginRequestId,
        status: verificationReady ? "ready" : "pending",
        codeSubmitted: statusConfirmsCodeSubmission,
        email: "new@example.test",
        plan: "Max",
      },
    });
  });
  const submittedCode = "FAKE-CLAUDE-CODE#FAKE-STATE";
  await claudeSignIn.getByLabel("Final authorization response").fill("123456");
  const rejectedEmailCode = waitForPost(
    page,
    "/api/accounts/claude/login/code",
    (body) => body.code === "123456",
  );
  await claudeSignIn
    .getByRole("button", { name: "Submit authorization response" })
    .click();
  expect((await rejectedEmailCode).ok()).toBe(false);
  await expect(
    claudeSignIn.getByLabel("Final authorization response"),
  ).toHaveValue("123456");
  await expect(claudeSignIn).toContainText(
    "Complete any email verification on Claude's sign-in page first.",
  );
  await claudeSignIn
    .getByLabel("Final authorization response")
    .fill(submittedCode);
  const lostCodeResponse = page.waitForEvent("requestfailed", (request) => {
    const url = new URL(request.url());
    return (
      url.pathname === "/api/accounts/claude/login/code" &&
      request.method() === "POST" &&
      request.postDataJSON().code === submittedCode
    );
  });
  await claudeSignIn
    .getByRole("button", { name: "Submit authorization response" })
    .click();
  await lostCodeResponse;
  expect(codeAttempts).toHaveLength(2);
  expect(codeAttempts[0].body.login_id).toBe(codeAttempts[1].body.login_id);
  expect(codeAttempts[0].requestId).toBe(codeAttempts[1].requestId);
  await expect(claudeSignIn).toContainText(
    "Studio could not confirm whether Claude received the authorization response.",
  );
  await expect(
    claudeSignIn.getByLabel("Final authorization response"),
  ).toBeHidden();
  const checkStatus = claudeSignIn.getByRole("button", {
    name: "Check sign-in status",
  });
  await expect(checkStatus).toBeVisible();
  await checkStatus.click();
  await expect(claudeSignIn).toContainText(
    "Studio could not confirm whether Claude received the authorization response.",
  );
  await expect(
    claudeSignIn.getByLabel("Final authorization response"),
  ).toBeHidden();
  expect(codeAttempts).toHaveLength(2);
  statusConfirmsCodeSubmission = true;
  await checkStatus.click();
  await expect(claudeSignIn).toContainText(
    "The full authorization response was submitted. Waiting for Claude to verify it; do not submit it again.",
  );
  // Keep the explicit recovery read pending until the manual refresh is dispatched.
  let releaseFinalStatus;
  finalStatusBarrier = new Promise((resolve) => {
    releaseFinalStatus = resolve;
  });
  try {
    await checkStatus.click();
    verificationReady = true;
  } finally {
    releaseFinalStatus();
  }
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
  const unknownSignInResponse = page.waitForResponse((response) => {
    const url = new URL(response.url());
    return (
      url.pathname === "/api/accounts/claude/login" &&
      response.request().method() === "GET" &&
      response.status() === 404
    );
  });
  let releaseUnknownStatus;
  let resolveUnknownStatusArrived;
  const unknownStatusRelease = new Promise((resolve) => {
    releaseUnknownStatus = resolve;
  });
  const unknownStatusArrived = new Promise((resolve) => {
    resolveUnknownStatusArrived = resolve;
  });
  await page.route("**/api/accounts/claude/login*", async (route) => {
    const requestUrl = new URL(route.request().url());
    if (
      route.request().method() !== "GET" ||
      requestUrl.searchParams.get("request_id") !== secondClaudeLoginId
    )
      return route.continue();
    const response = await route.fetch();
    if (response.status() === 404) {
      resolveUnknownStatusArrived();
      await unknownStatusRelease;
    }
    await route.fulfill({ response });
  });
  await secondClaudeSignIn
    .getByRole("button", { name: "Start sign-in", exact: true })
    .click();
  await secondClaudeStartAborted;
  await unknownStatusArrived;
  await expect(
    secondClaudeSignIn.getByRole("button", {
      name: "Retry sign-in",
      exact: true,
    }),
  ).toBeVisible();
  await page.clock.pauseAt(new Date(pageClockStart.getTime() + 60_000));
  releaseUnknownStatus();
  await unknownSignInResponse;
  const startAgain = secondClaudeSignIn.getByRole("button", {
    name: "Start again",
    exact: true,
  });
  await page.clock.fastForward(4_999);
  await expect(startAgain).not.toBeVisible();
  await page.clock.fastForward(1);
  await expect(startAgain).toBeVisible();
  await startAgain.click();
  await page.clock.resume();
  await expect(
    secondClaudeSignIn.getByRole("link", { name: "Open sign-in page" }),
  ).toBeVisible();
  expect(addStarts[3]?.loginId).not.toBe(addStarts[2]?.loginId);
  expect(addStarts[3]?.requestId).not.toBe(addStarts[2]?.requestId);
  const activeClaudeLoginId = addStarts[3]?.loginId;
  expect(activeClaudeLoginId).toEqual(expect.any(String));
  const claudeCancelResponse = waitForPost(
    page,
    "/api/accounts/claude/login/cancel",
    (body) => body.login_id === activeClaudeLoginId,
  );
  await secondClaudeSignIn
    .getByRole("button", { name: "Cancel", exact: true })
    .click();
  expect((await claudeCancelResponse).ok()).toBe(true);
  await expect(secondClaudeSignIn).toContainText("Sign-in cancelled.");
  await secondClaudeSignIn.getByLabel("Close", { exact: true }).click();
  await expect(secondClaudeSignIn).toBeHidden();
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
  const codexCancelResponse = waitForPost(page, "/api/accounts/login/cancel");
  await codexSignIn.getByRole("button", { name: "Cancel sign-in" }).click();
  expect((await codexCancelResponse).ok()).toBe(true);
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
  await work.getByRole("button", { name: /^Limit details for/ }).click();
  await work.getByRole("button", { name: /Remote · sign in again/ }).click();
  const reconnect = remoteFrame.getByRole("dialog", {
    name: /Sign in to Codex · person@example.test · Remote/,
  });
  const reconnectResponse = waitForPost(
    page,
    "/api/accounts/login",
    (body) => body.account_key === "remote-codex",
  );
  await reconnect.getByRole("button", { name: "Start sign-in" }).click();
  expect((await reconnectResponse).ok()).toBe(true);
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
  await expect(
    work.getByRole("button", { name: "Actions for person@example.test" }),
  ).toBeFocused();
});
