import { test, expect } from "../playwright.mjs";
import { fixture } from "./multi-server-fixture.mjs";
async function settings(page) {
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  await page.getByRole("tab", { name: "Servers", exact: true }).click();
  return page.getByRole("dialog", { name: "Studio settings", exact: true });
}
test("revoke cancels an automatic invitation whose reply is still in flight", async ({
  page,
  context,
}) => {
  const local = await fixture("Local"),
    remote = await fixture("Remote", true);
  let release;
  const held = new Promise((resolve) => {
    release = resolve;
  });
  let invited = false;
  try {
    await context.addInitScript(
      ({ origin, destination }) => {
        const native = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init),
            url = new URL(request.url);
          if (url.origin !== origin) return native(request);
          return native(
            new Request(destination + url.pathname + url.search, request),
          );
        };
      },
      { origin: remote.invitation.origin, destination: remote.origin },
    );
    await page.route(local.origin + "/api/multi-server", async (route) => {
      if (route.request().postDataJSON()?.action !== "ui_invite")
        return route.continue();
      const response = await route.fetch();
      invited = true;
      await held;
      await route.fulfill({ response });
    });
    local.discoverPeer(remote);
    await page.route(local.origin + "/legacy-seed", (route) =>
      route.fulfill({
        contentType: "text/html",
        body: "<html><body>Seed</body></html>",
      }),
    );
    await page.goto(local.origin + "/legacy-seed");
    await page.evaluate(async (invitation) => {
      const db = await new Promise((resolve, reject) => {
        const request = indexedDB.open("studio-automatic-ui-access-v1", 1);
        request.onupgradeneeded = () =>
          request.result.createObjectStore("attempts");
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
      });
      await new Promise((resolve, reject) => {
        const tx = db.transaction("attempts", "readwrite");
        tx.objectStore("attempts").put(
          {
            localServerId: "local",
            serverId: "remote",
            origin: invitation.origin,
            generation: "old",
            inviteRequestId: "old-invite",
            pairRequestId: "old-pair",
            invitation,
          },
          "legacy",
        );
        tx.oncomplete = resolve;
        tx.onerror = reject;
      });
      db.close();
    }, remote.invitation);
    await page.goto(local.origin);
    await expect.poll(() => invited).toBe(true);
    const dialog = await settings(page);
    await dialog
      .locator("[data-settings-server=remote]")
      .getByRole("button", { name: "Revoke", exact: true })
      .click();
    await page
      .getByRole("dialog", { name: "Revoke server access", exact: true })
      .getByRole("button", { name: "Revoke", exact: true })
      .click();
    await expect
      .poll(() =>
        local.accessRequests.some(
          (row) => row.action === "revoke" && row.clientId === "remote",
        ),
      )
      .toBe(true);
    await expect(dialog.locator("[data-settings-server=remote]")).toContainText(
      "Revoked",
    );
    release();
    await expect
      .poll(async () => {
        const value = await page.evaluate(async () => {
          const db = await new Promise((resolve, reject) => {
            const request = indexedDB.open("studio-automatic-ui-access-v1");
            request.onsuccess = () => resolve(request.result);
            request.onerror = () => reject(request.error);
          });
          const rows = await new Promise((resolve) => {
            const request = db
              .transaction("attempts")
              .objectStore("attempts")
              .getAll();
            request.onsuccess = () => resolve(request.result);
          });
          db.close();
          return rows;
        });
        expect(JSON.stringify(value)).not.toContain('"token"');
        expect(JSON.stringify(value)).not.toContain("secret-remote");
        return value.some((row) => row.generation !== "old" && row.invitation);
      })
      .toBe(true);
    expect(remote.pairs).toHaveLength(0);
    expect(
      await page.evaluate(() =>
        JSON.parse(localStorage.getItem("studio-paired-servers-v1") || "[]"),
      ),
    ).toEqual([]);
  } finally {
    release();
    await local.close();
    await remote.close();
  }
});
test("another tab cannot clear an uncertain management identity", async ({
  page,
  context,
}) => {
  const local = await fixture("Local");
  const other = await context.newPage();
  try {
    await page.addInitScript(() => {
      const native = window.fetch.bind(window);
      let lost = false;
      window.fetch = async (input, init) => {
        const request = new Request(input, init);
        const action =
          new URL(request.url).pathname === "/api/multi-server" &&
          request.method === "POST"
            ? (await request.clone().json()).action
            : null;
        const response = await native(request);
        if (action === "discover" && !lost) {
          lost = true;
          await response.text();
          throw new TypeError("Discover response lost in fixture");
        }
        return response;
      };
    });
    await Promise.all([page.goto(local.origin), other.goto(local.origin)]);
    const a = await settings(page),
      b = await settings(other);
    await a
      .getByRole("button", { name: "Find servers now", exact: true })
      .click();
    await expect(a.getByRole("alert")).toContainText("Discover response lost");
    await b.getByRole("switch", { name: "Pair servers automatically" }).click();
    await expect(b.getByRole("alert")).toContainText(
      "Retry the saved server request",
    );
    expect(
      local.accessRequests.filter((row) => row.action === "settings"),
    ).toHaveLength(0);
    const pending = await page.evaluate(() =>
      Object.keys(localStorage).filter((key) =>
        key.startsWith("studio-server-management-v1:"),
      ),
    );
    expect(pending).toHaveLength(1);
    const first = local.accessRequests.find((row) => row.action === "discover");
    await a
      .getByRole("button", { name: "Retry saved request", exact: true })
      .click();
    await expect
      .poll(
        () =>
          local.accessRequests.filter((row) => row.action === "discover")
            .length,
      )
      .toBe(2);
    expect(
      local.accessRequests.filter((row) => row.action === "discover"),
    ).toEqual([first, first]);
    await expect
      .poll(() =>
        page.evaluate(
          () =>
            Object.keys(localStorage).filter((key) =>
              key.startsWith("studio-server-management-v1:"),
            ).length,
        ),
      )
      .toBe(0);
  } finally {
    await other.close();
    await local.close();
  }
});
