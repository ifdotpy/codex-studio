import { test, expect } from "../playwright.mjs";
import { fixture } from "./multi-server-fixture.mjs";

async function settings(page) {
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  await page.getByRole("tab", { name: "Servers", exact: true }).click();
  return page.getByRole("dialog", { name: "Studio settings", exact: true });
}

test("a discovered Serve HTTPS port preserves server rows and alias save", async ({
  page,
  context,
}) => {
  const local = await fixture("Local");
  const remote = await fixture("Remote", true);
  remote.invitation.origin += ":8443";
  local.discoverPeer(remote);
  try {
    await context.addInitScript(
      ({ origin, destination }) => {
        const native = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init),
            url = new URL(request.url);
          if (url.origin !== origin) return native(request);
          const bytes = await request.clone().arrayBuffer();
          return native(destination + url.pathname + url.search, {
            method: request.method,
            headers: request.headers,
            ...(bytes.byteLength ? { body: bytes } : {}),
            signal: request.signal,
            redirect: "error",
            credentials: "omit",
          });
        };
      },
      { origin: remote.invitation.origin, destination: remote.origin },
    );
    await page.goto(local.origin);
    const dialog = await settings(page);
    await expect(dialog.locator("[data-settings-server=remote]")).toContainText(
      remote.invitation.origin,
    );
    const row = dialog.locator("[data-settings-server=local]");
    await row.getByRole("textbox", { name: "Server alias" }).fill("LUM");
    await row.getByRole("button", { name: "Save alias", exact: true }).click();
    await expect.poll(() => local.aliases.local).toBe("LUM");
    await expect(
      row.getByRole("button", { name: "Save alias", exact: true }),
    ).toBeDisabled();
    await expect(dialog).not.toContainText("The alias result is unknown");
    await expect(dialog).not.toContainText(
      "Enter the Tailscale Serve HTTPS address",
    );
    await expect.poll(() => remote.pairs.length).toBe(1);
    expect(
      local.accessRequests.filter((body) => body.action === "alias"),
    ).toHaveLength(1);
  } finally {
    await local.close();
    await remote.close();
  }
});

test("a confirmed alias save stays confirmed when the following read fails", async ({
  page,
}) => {
  let failNextRead = false;
  const local = await fixture("Local", false, {
    handle({ request, url, body, json }) {
      if (url.pathname !== "/api/multi-server") return false;
      if (body.action === "alias") failNextRead = true;
      if (request.method === "GET" && failNextRead) {
        failNextRead = false;
        json({ error: "Refresh failed in fixture" }, 503);
        return true;
      }
      return false;
    },
  });
  try {
    await page.goto(local.origin);
    const dialog = await settings(page);
    const row = dialog.locator("[data-settings-server=local]");
    await row.getByRole("textbox", { name: "Server alias" }).fill("LUM");
    await row.getByRole("button", { name: "Save alias", exact: true }).click();
    await expect.poll(() => local.aliases.local).toBe("LUM");
    await expect(dialog.getByRole("alert")).toContainText(
      "Refresh failed in fixture",
    );
    await expect(dialog).not.toContainText("The alias result is unknown");
    expect(
      await page.evaluate(() =>
        Object.keys(localStorage).filter((key) =>
          key.startsWith("studio-server-management-v1:"),
        ),
      ),
    ).toEqual([]);
    await page.evaluate(() =>
      window.dispatchEvent(new Event("studio-server-discovery-refresh")),
    );
    await expect(
      row.getByRole("button", { name: "Save alias", exact: true }),
    ).toBeDisabled();
    expect(
      local.accessRequests.filter((body) => body.action === "alias"),
    ).toHaveLength(1);
  } finally {
    await local.close();
  }
});
