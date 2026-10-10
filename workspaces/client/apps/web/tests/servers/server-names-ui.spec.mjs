import { test, expect } from "../playwright.mjs";
import { fixture } from "./multi-server-fixture.mjs";

async function settings(page) {
  await page
    .getByRole("button", { name: "Studio settings", exact: true })
    .click();
  await page.getByRole("tab", { name: "Servers", exact: true }).click();
  return page.getByRole("dialog", { name: "Studio settings", exact: true });
}

test("server cards rename all four names and retain aliases after reload", async ({
  page,
  context,
}, testInfo) => {
  test.setTimeout(90000);
  const local = await fixture("Local");
  local.aliases.local = local.aliases[local.invitation.serverId] = "LUM";
  const remotes = [
    await fixture("MBP", true),
    await fixture("WSL", true),
    await fixture("WIN", true),
  ];
  for (const remote of remotes) local.discoverPeer(remote);
  try {
    await context.addInitScript(
      ({ destinations }) => {
        const native = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init),
            url = new URL(request.url);
          if (!destinations[url.origin]) return native(request);
          const bytes = await request.clone().arrayBuffer();
          return native(destinations[url.origin] + url.pathname + url.search, {
            method: request.method,
            headers: request.headers,
            ...(bytes.byteLength ? { body: bytes } : {}),
            signal: request.signal,
            redirect: "error",
            credentials: "omit",
          });
        };
      },
      {
        destinations: Object.fromEntries(
          remotes.map((remote) => [remote.invitation.origin, remote.origin]),
        ),
      },
    );
    await page.goto(local.origin);
    let dialog = await settings(page);
    for (const remote of remotes)
      await expect.poll(() => remote.pairs.length).toBe(1);
    const aliases = await dialog
      .getByRole("textbox", { name: "Server alias", exact: true })
      .evaluateAll((inputs) => inputs.map((input) => input.value));
    const names = {
      local: "Lumina Mac",
      mbp: "Igor MBP",
      wsl: "Kukuka WSL",
      win: "Kukuka Windows",
    };
    for (const [id, name] of Object.entries(names)) {
      const card = dialog.locator(`[data-settings-server=${id}]`);
      await card
        .getByRole("textbox", { name: "Server name", exact: true })
        .fill(name);
      await card
        .getByRole("button", { name: "Save name", exact: true })
        .click();
      await expect(card.getByRole("heading", { level: 3 })).toContainText(name);
      await expect(
        card.getByRole("button", { name: "Save name", exact: true }),
      ).toBeDisabled();
    }
    expect(
      await dialog
        .getByRole("textbox", { name: "Server alias", exact: true })
        .evaluateAll((inputs) => inputs.map((input) => input.value))
        .then((values) => values.sort()),
    ).toEqual(aliases.sort());
    expect(
      local.accessRequests.filter((body) => body.action === "name"),
    ).toHaveLength(4);
    const card = dialog.locator("[data-settings-server=local]");
    await card
      .getByRole("textbox", { name: "Server name", exact: true })
      .fill("bad\u202ename");
    await card.getByRole("button", { name: "Save name", exact: true }).click();
    await expect(card).toContainText(
      "Use 1 to 80 visible characters without outer spaces.",
    );
    expect(
      local.accessRequests.filter((body) => body.action === "name"),
    ).toHaveLength(4);
    await page.reload();
    dialog = await settings(page);
    for (const name of Object.values(names))
      await expect(dialog).toContainText(name);
    expect(
      await page.evaluate(() =>
        JSON.parse(localStorage.getItem("studio-paired-servers-v1"))
          .map((row) => row.label)
          .sort(),
      ),
    ).toEqual(Object.values(names).slice(1).sort());
    await page.setViewportSize({ width: 1440, height: 1800 });
    const path = testInfo.outputPath("server-names.png");
    await dialog.screenshot({ path });
    await testInfo.attach("Server names", { path, contentType: "image/png" });
    await page.setViewportSize({ width: 390, height: 844 });
    for (const name of Object.values(names))
      await expect(dialog).toContainText(name);
    const longName = "A".repeat(80);
    const longCard = dialog.locator("[data-settings-server=local]");
    await longCard
      .getByRole("textbox", { name: "Server name", exact: true })
      .fill(longName);
    await longCard
      .getByRole("button", { name: "Save name", exact: true })
      .click();
    await expect(longCard.getByRole("heading", { level: 3 })).toContainText(
      longName,
    );
    expect(
      await dialog.evaluate(
        (element) => element.scrollWidth <= element.clientWidth + 1,
      ),
    ).toBe(true);
  } finally {
    await local.close();
    for (const remote of remotes) await remote.close();
  }
});

test("a UI-only client renames through its signed credential and retries the same request", async ({
  page,
  context,
}) => {
  test.setTimeout(60000);
  const local = await fixture("Local");
  const remote = await fixture("Remote", true);
  try {
    await context.addInitScript(
      ({ origin, destination }) => {
        const native = window.fetch.bind(window);
        window.fetch = async (input, init) => {
          const request = new Request(input, init),
            url = new URL(request.url);
          if (url.origin !== origin) return native(request);
          const bytes = await request.clone().arrayBuffer();
          const response = await native(
            destination + url.pathname + url.search,
            {
              method: request.method,
              headers: request.headers,
              ...(bytes.byteLength ? { body: bytes } : {}),
              signal: request.signal,
              redirect: "error",
              credentials: "omit",
            },
          );
          if (
            request.method === "POST" &&
            url.pathname === "/api/multi-server" &&
            JSON.parse(new TextDecoder().decode(bytes)).action === "name" &&
            !sessionStorage.getItem("name-response-lost")
          ) {
            sessionStorage.setItem("name-response-lost", "1");
            await response.text();
            throw new TypeError("The name response was lost.");
          }
          return response;
        };
      },
      { origin: remote.invitation.origin, destination: remote.origin },
    );
    await page.goto(local.origin + "/?studio-ui-only=1");
    const dialog = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
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
    const card = dialog.locator("[data-settings-server=remote]");
    await card
      .getByRole("textbox", { name: "Server name", exact: true })
      .fill("Igor MBP");
    await card.getByRole("button", { name: "Save name", exact: true }).click();
    await expect(
      dialog.getByRole("button", { name: "Retry saved request", exact: true }),
    ).toBeVisible();
    await dialog
      .getByRole("button", { name: "Retry saved request", exact: true })
      .click();
    await expect(card.getByRole("heading", { level: 3 })).toHaveText(
      "Igor MBP",
    );
    await expect(
      dialog.getByRole("button", { name: "Retry saved request", exact: true }),
    ).toHaveCount(0);
    const requests = remote.accessRequests.filter(
      (body) => body.action === "name",
    );
    expect(requests).toHaveLength(2);
    expect(requests[0]).toEqual(requests[1]);
    expect(requests[0].serverId).toBe("local");
    expect(
      local.accessRequests.filter((body) => body.action === "name"),
    ).toHaveLength(0);
  } finally {
    await local.close();
    await remote.close();
  }
});
