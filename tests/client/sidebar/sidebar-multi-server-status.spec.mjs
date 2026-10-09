import { test, expect } from "../playwright.mjs";
import { fixture } from "../servers/multi-server-fixture.mjs";

test("paired sidebar shows the chat provider and server alias", async ({
  page,
  context,
}) => {
  test.setTimeout(180000);
  const local = await fixture("Local");
  const remote = await fixture("Remote", true);
  const row = (id, name, updated) => ({
    id,
    name,
    updated,
    created: updated,
    provider: "claude",
    source: "managed",
    rootId: id,
    isLead: true,
    cwd: "/same/project",
    status: "waiting",
    inFlight: false,
    archived: false,
    deletedAt: null,
  });
  remote.setChats([
    row("recent", "Claude on remote", Date.now() / 1000),
    row("old", "Old remote chat", 1),
    row("old-two", "Second old remote chat", 1),
  ]);
  try {
    await context.addInitScript(
      ({ origin, target }) => {
        const nativeFetch = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init);
          const url = new URL(request.url);
          if (url.origin !== origin) return nativeFetch(request);
          const bytes = await request.clone().arrayBuffer();
          return nativeFetch(target + url.pathname + url.search, {
            method: request.method,
            headers: request.headers,
            ...(bytes.byteLength ? { body: bytes } : {}),
            signal: request.signal,
            redirect: "error",
            credentials: "omit",
          });
        };
      },
      { origin: remote.invitation.origin, target: remote.origin },
    );
    await page.goto(local.origin + "/?studio-navigation=combined");
    await page.locator("#message").waitFor();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await page.getByRole("tab", { name: "Servers", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Studio settings" });
    await dialog
      .getByRole("button", { name: "Add with an invitation", exact: true })
      .click();
    await dialog.getByLabel("Server address").fill(remote.invitation.origin);
    await dialog
      .getByLabel("Pairing invitation")
      .fill(JSON.stringify(remote.invitation));
    await dialog
      .getByRole("button", { name: "Pair server", exact: true })
      .click();
    await expect(page.locator('[data-settings-server="remote"]')).toBeVisible();
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
    const group = page
      .locator(".server-sidebar .sidebar-project")
      .filter({ has: page.locator('[data-chat="recent"]') });
    const chat = group.locator('[data-chat="recent"]');
    await expect(chat.locator(".chat-server-line")).toHaveText("REM");
    await expect(chat.locator(".chat-server-line")).toHaveAttribute(
      "aria-label",
      "Claude, REM",
    );
    await expect(group.locator('[data-chat="old"]')).toHaveCount(0);
    await group
      .getByRole("button", { name: "Show more (2)", exact: true })
      .click();
    await expect(group.locator('[data-chat="old"]')).toBeVisible();
    await expect(
      group.getByRole("button", { name: "Show less", exact: true }),
    ).toBeVisible();
    await page.reload();
    await expect(group.locator('[data-chat="old"]')).toBeVisible();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const card = page.locator('[data-settings-server="remote"]');
    await card.getByLabel("Server alias", { exact: true }).fill("MAC");
    await card.getByRole("button", { name: "Save alias", exact: true }).click();
    await expect(
      card.getByText("This alias belongs to another server."),
    ).toBeVisible();
    await card.getByLabel("Server alias", { exact: true }).fill("SRV");
    await card.getByRole("button", { name: "Save alias", exact: true }).click();
    await expect(
      card.getByRole("button", { name: "Save alias", exact: true }),
    ).toBeDisabled();
    await dialog.getByRole("button", { name: "Close", exact: true }).click();
    await expect(chat.locator(".chat-server-line")).toHaveText("SRV");
    await group.getByRole("button", { name: "Show less", exact: true }).click();
    await expect(group.locator('[data-chat="old"]')).toHaveCount(0);
    await page.reload();
    await expect(chat.locator(".chat-server-line")).toHaveText("SRV");
    await expect(group.locator('[data-chat="old"]')).toHaveCount(0);
  } finally {
    await Promise.all([local.close(), remote.close()]);
  }
});
