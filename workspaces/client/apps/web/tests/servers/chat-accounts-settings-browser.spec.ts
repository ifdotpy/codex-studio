import { fixturePostBody } from "./browser-fixture.ts";
import { remoteNetwork } from "./browser-fixture.ts";
import { test, expect } from "../playwright.ts";
import { fixture } from "./multi-server-fixture.ts";

test("Accounts includes a backend-paired server before this UI has credentials", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(120000);
  page.setDefaultTimeout(15000);
  const local = await fixture("Local", false, [
    {
      id: "default",
      label: "Work",
      provider: "codex",
      email: "person@example.test",
      status: "ready",
      isDefault: true,
    },
  ]);
  const remote = await fixture("Remote", true, []);
  try {
    await remoteNetwork(context, remote);
    local.discoverPeer(remote);
    await page.addInitScript(() => {
      if (window === window.top)
        localStorage.setItem("studio-automatic-ui-removals-v1", '["remote"]');
    });
    await page.goto(local.origin + "/?studio-navigation=classic");
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .waitFor();
    expect(
      await page.evaluate(() =>
        JSON.parse(localStorage.getItem("studio-paired-servers-v1") || "[]"),
      ),
    ).toEqual([]);
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const settings = page.getByRole("dialog", { name: "Studio settings" });
    await settings.getByRole("tab", { name: "Accounts", exact: true }).click();
    const accounts = settings.getByRole("region", {
      name: "Accounts",
      exact: true,
    });
    await expect(
      accounts.getByRole("button", { name: "+ Remote", exact: true }),
    ).toBeVisible();
    expect(remote.pairs).toHaveLength(0);
    await page.screenshot({
      path: testInfo.outputPath("backend-paired-accounts.png"),
    });
    await testInfo.attach("Backend-paired Accounts before UI attachment", {
      path: testInfo.outputPath("backend-paired-accounts.png"),
      contentType: "image/png",
    });
    await accounts
      .getByRole("button", { name: "+ Remote", exact: true })
      .click();
    const frame = page;
    const signIn = frame.getByRole("dialog", {
      name: "Sign in to Codex · Work",
    });
    await expect(signIn).toBeVisible({ timeout: 20000 });
    await expect(signIn.getByLabel("Server", { exact: true })).toHaveValue(
      "Remote",
    );
    await expect(signIn).toContainText("person@example.test");
    expect(remote.pairs).toHaveLength(1);
    await signIn.screenshot({
      path: testInfo.outputPath("backend-paired-remote-sign-in.png"),
    });
    await testInfo.attach("Remote sign-in after automatic UI attachment", {
      path: testInfo.outputPath("backend-paired-remote-sign-in.png"),
      contentType: "image/png",
    });
    await signIn.getByRole("button", { name: "Sign in", exact: true }).click();
    await expect(
      signIn.getByRole("button", { name: "Cancel sign-in" }),
    ).toBeVisible();
    const loginId = remote.accountWrites
      .filter((row) => row.path === "/api/accounts/login")
      .at(-1)?.loginId;
    expect(loginId).toEqual(expect.any(String));
    const cancelResponse = page.waitForResponse((response) => {
      const request = response.request();
      return (
        new URL(response.url()).pathname === "/api/accounts/login/cancel" &&
        request.method() === "POST" &&
        fixturePostBody<"/api/accounts/claude/login">(request.postDataJSON())
          .login_id === loginId
      );
    });
    await signIn.getByRole("button", { name: "Cancel sign-in" }).click();
    expect((await cancelResponse).ok()).toBe(true);
    await expect(signIn).toContainText("Sign-in cancelled.");
    expect(remote.accountWrites.map((row) => row.path)).toEqual([
      "/api/accounts/login",
      "/api/accounts/login/cancel",
    ]);
    expect(local.accountWrites).toEqual([]);
    await signIn.getByLabel("Close", { exact: true }).click();
    await expect(settings).toBeVisible();
    await expect(
      accounts.getByRole("button", { name: "+ Remote", exact: true }),
    ).toBeVisible();
    await page.screenshot({
      path: testInfo.outputPath("backend-paired-accounts-attached.png"),
    });
    await testInfo.attach("Backend-paired Accounts after UI attachment", {
      path: testInfo.outputPath("backend-paired-accounts-attached.png"),
      contentType: "image/png",
    });
  } finally {
    await remote.close();
    await local.close();
  }
});

test("chat Accounts settings show all servers and route remote sign-in", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(120000);
  page.setDefaultTimeout(15000);
  const local = await fixture("Local", false, [
    {
      id: "default",
      label: "Work",
      provider: "codex",
      email: "person@example.test",
      status: "ready",
      isDefault: true,
    },
  ]);
  const remote = await fixture("Remote", true, []);
  try {
    await remoteNetwork(context, remote);
    await page.goto(local.origin + "/?studio-navigation=classic");
    await page.locator("#message").waitFor();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const settings = page.getByRole("dialog", { name: "Studio settings" });
    const accounts = settings.getByRole("region", {
      name: "Accounts",
      exact: true,
    });
    // The standalone view keeps its local account panel.
    await expect(
      accounts.getByRole("button", { name: "✓ Local" }),
    ).toBeVisible();
    await expect(
      accounts.getByRole("button", { name: "+ Remote" }),
    ).toHaveCount(0);
    await settings.getByRole("tab", { name: "Servers", exact: true }).click();
    await settings
      .getByRole("button", { name: "Add with an invitation", exact: true })
      .click();
    await settings.getByLabel("Server address").fill(remote.invitation.origin);
    await settings
      .getByLabel("Pairing invitation")
      .fill(JSON.stringify(remote.invitation));
    await settings
      .getByRole("button", { name: "Pair server", exact: true })
      .click();
    await expect(page.locator('[data-settings-server="remote"]')).toBeVisible();
    await settings.getByRole("button", { name: "Close", exact: true }).click();

    const localFrame = page;
    const remoteFrame = page;
    await localFrame.getByRole("button", { name: /^Local chat/ }).click();
    await localFrame.locator("#message").waitFor();
    await localFrame
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await settings.getByRole("tab", { name: "Accounts", exact: true }).click();
    await expect(
      accounts.getByRole("button", { name: "+ Remote", exact: true }),
    ).toBeVisible();
    await expect(settings).toBeVisible();
    await expect(
      page.getByRole("dialog", { name: "Studio settings" }),
    ).toHaveCount(1);
    await expect(
      settings.getByRole("tab", { name: "Accounts", exact: true }),
    ).toHaveAttribute("aria-selected", "true");
    await expect(
      accounts.getByRole("button", { name: "✓ Local" }),
    ).toBeVisible();
    await page.screenshot({
      path: testInfo.outputPath("chat-accounts-all-servers.png"),
    });
    await testInfo.attach("Chat Accounts, all servers", {
      path: testInfo.outputPath("chat-accounts-all-servers.png"),
      contentType: "image/png",
    });

    await accounts
      .getByRole("button", { name: "+ Remote", exact: true })
      .click();
    const signIn = remoteFrame.getByRole("dialog", {
      name: "Sign in to Codex · Work",
    });
    await expect(signIn).toBeVisible();
    await expect(settings).toBeHidden();
    await expect(
      page.getByRole("dialog", { name: /Sign in to Codex/ }),
    ).toHaveCount(1);
    await signIn.getByLabel("Close", { exact: true }).click();
    await expect(settings).toBeVisible();
    await expect(
      accounts.getByRole("button", { name: "+ Remote", exact: true }),
    ).toBeVisible();
    await settings.getByRole("button", { name: "Close", exact: true }).click();

    // A remote chat opens the same shell Accounts panel from another tab.
    await remoteFrame
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await settings.getByRole("tab", { name: "Accounts", exact: true }).click();
    await expect(settings).toBeVisible();
    await expect(
      accounts.getByRole("button", { name: "+ Remote", exact: true }),
    ).toBeVisible();
    await settings
      .getByRole("tab", { name: "Appearance", exact: true })
      .click();
    const remoteSettings = remoteFrame.getByRole("dialog", {
      name: "Studio settings",
    });
    await expect(
      remoteSettings.getByRole("tab", { name: "Appearance", exact: true }),
    ).toHaveAttribute("aria-selected", "true");
    await remoteSettings
      .getByRole("tab", { name: "Accounts", exact: true })
      .click();
    await expect(settings).toBeVisible();
    await expect(
      page.getByRole("dialog", { name: "Studio settings" }),
    ).toHaveCount(1);
    await expect(
      accounts.getByRole("button", { name: "+ Remote", exact: true }),
    ).toBeVisible();
    expect(remote.accountWrites).toEqual([]);
    expect(local.accountWrites).toEqual([]);
  } finally {
    await remote.close();
    await local.close();
  }
});
