import { test, expect, spawnFixture, readTestState } from "../playwright.mjs";
import { mkdtemp, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test("sidebar status, alias validation, and compact choices", async ({
  page,
}, testInfo) => {
  test.setTimeout(600000);
  const root = resolve(import.meta.dirname, "../../../../../../");
  const stateDir = await mkdtemp(join(tmpdir(), "studio-sidebar-status-"));
  const evidence =
    process.env.SIDEBAR_SCREENSHOTS || testInfo.outputPath("screenshots");
  await mkdir(evidence, { recursive: true });
  const proc = spawnFixture(
    "python3",
    ["-B", join(root, "tests/sidebar-drag-fixture.py"), stateDir],
    {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, SIDEBAR_ALIAS_FIXTURE: "1" },
    },
  );
  let log = "";
  proc.stderr.on("data", (bytes) => {
    log += bytes;
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    const port = await new Promise((done, reject) => {
      proc.stdout.once("data", (bytes) => done(Number(String(bytes).trim())));
      proc.once("exit", () => reject(new Error(log)));
    });
    const url = `http://127.0.0.1:${port}`;
    const initial = await readTestState(url);
    const project = initial.runtime.projects.find(
      (row) => row.name === "Project A",
    );
    const chats = Object.fromEntries(
      initial.runtime.agents.map((row) => [row.name, row]),
    );
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.emulateMedia({ colorScheme: "light" });
    await page.addInitScript(
      ({ stateDir, id }) => {
        const key = `codex-desktop-opened:${stateDir}`;
        if (!localStorage.getItem(key))
          localStorage.setItem(key, JSON.stringify(id));
      },
      { stateDir: initial.stateDir, id: chats.Recent.id },
    );
    await page.goto(url);
    const sidebar = page.locator("#sidebar");
    const group = page.locator(`[data-project-path="${project.path}"]`);
    const chat = (name) => sidebar.locator(`[data-chat="${chats[name].id}"]`);
    await expect(
      group.getByRole("button", { name: /^Show more \(\d+\)$/ }),
    ).toBeVisible();
    await expect(chat("Old hidden")).toHaveCount(0);
    await expect(chat("Old folder hidden")).toHaveCount(0);
    for (const name of [
      "Team source",
      "Team peer",
      "Pinned",
      "Recent",
      "Unread",
      "Running",
    ])
      await expect(chat(name)).toBeVisible();
    const line = chat("Running").locator(".chat-server-line");
    await expect(line).toHaveText("MAC");
    await expect(line.locator("svg")).toHaveCount(1);
    await expect(chat("Recent").locator(".chat-server-line")).toHaveAttribute(
      "aria-label",
      "Claude, MAC",
    );
    expect(await line.evaluate((node) => getComputedStyle(node).fontSize)).toBe(
      "11px",
    );
    expect(
      await line
        .locator("svg")
        .evaluate((node) => node.getBoundingClientRect().width),
    ).toBe(11);
    await expect(
      chat("Running").locator('[data-chat-status="working"]'),
    ).toBeVisible();
    await expect(
      group.locator(":scope > .project-tree-heading .chat-server-line"),
    ).toHaveCount(0);
    const hiddenCount = await group
      .getByRole("button", { name: /^Show more \(\d+\)$/ })
      .innerText();
    expect(hiddenCount).toBe("Show more (3)");
    await group.getByRole("button", { name: hiddenCount, exact: true }).click();
    await expect(chat("Old hidden")).toBeVisible();
    await expect(chat("Old folder hidden")).toBeVisible();
    await expect(
      group.getByRole("button", { name: "Show less", exact: true }),
    ).toBeVisible();
    await page.reload();
    await expect(chat("Old hidden")).toBeVisible();
    await group.getByRole("button", { name: "Show less", exact: true }).click();
    await expect(chat("Old hidden")).toHaveCount(0);
    await page.reload();
    await expect(chat("Old hidden")).toHaveCount(0);
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await page.getByRole("tab", { name: "Servers", exact: true }).click();
    const card = page.locator('[data-settings-server="local"]');
    await card.getByLabel("Server alias", { exact: true }).fill("MBP");
    await card.getByRole("button", { name: "Save alias", exact: true }).click();
    await expect(
      card.getByText("This alias belongs to another server."),
    ).toBeVisible();
    await card.getByLabel("Server alias", { exact: true }).fill("ABCD");
    await card.getByRole("button", { name: "Save alias", exact: true }).click();
    await expect(
      card.getByText("Use 1 to 3 uppercase letters.", { exact: true }),
    ).toHaveCount(2);
    await card.getByLabel("Server alias", { exact: true }).fill("desk");
    await card.getByRole("button", { name: "Save alias", exact: true }).click();
    await expect(
      card.getByText("Use 1 to 3 uppercase letters.", { exact: true }),
    ).toHaveCount(2);
    await card.getByLabel("Server alias", { exact: true }).fill("DES");
    const aliasResponse = page.waitForResponse(
      (response) =>
        new URL(response.url()).pathname === "/api/multi-server" &&
        response.request().method() === "POST" &&
        response.request().postDataJSON()?.action === "alias",
    );
    await card.getByRole("button", { name: "Save alias", exact: true }).click();
    expect((await (await aliasResponse).json()).settings.aliases.local).toBe(
      "DES",
    );
    await expect(
      card.getByRole("button", { name: "Save alias", exact: true }),
    ).toBeDisabled();
    const serverState = await page.request.get(url + "/api/multi-server", {
      headers: { "X-Canvas-Token": initial.token },
    });
    expect((await serverState.json()).settings.aliases.local).toBe("DES");
    await page
      .getByRole("dialog", { name: "Studio settings" })
      .getByRole("button", { name: "Close", exact: true })
      .click();
    await expect(line).toHaveText("DES");
    await page.screenshot({
      animations: "disabled",
      path: join(evidence, "sidebar-light.png"),
    });
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await page.getByRole("tab", { name: "Appearance", exact: true }).click();
    await page.getByRole("radio", { name: "Dark", exact: true }).check();
    await page
      .getByRole("dialog", { name: "Studio settings" })
      .getByRole("button", { name: "Close", exact: true })
      .click();
    await expect(page.locator("html")).toHaveAttribute(
      "data-mantine-color-scheme",
      "dark",
    );
    await page.screenshot({
      animations: "disabled",
      path: join(evidence, "sidebar-dark.png"),
    });
    await page.setViewportSize({ width: 390, height: 844 });
    await page
      .getByRole("button", { name: "Toggle conversations", exact: true })
      .click();
    await expect(line).toBeVisible();
    await page.screenshot({
      animations: "disabled",
      path: join(evidence, "sidebar-mobile.png"),
    });
    expect(errors).toEqual([]);
    for (const name of ["light", "dark", "mobile"])
      await testInfo.attach(`sidebar-${name}`, {
        path: join(evidence, `sidebar-${name}.png`),
        contentType: "image/png",
      });
  } finally {
    proc.kill("SIGTERM");
  }
});
