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
      window.addEventListener("message", (event) => {
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
  await page.goto(local.origin);
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
      page.locator(`[data-server="${server.invitation.serverId}"]`),
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
  await expect(work.getByText("Default · This computer")).toBeVisible();
  await expect(
    work.getByRole("button", { name: "✓ This computer" }),
  ).toBeVisible();
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
    new URL("../../../docs/screenshots/", import.meta.url),
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
});
