import { test, expect, spawnFixture, readTestState } from "../playwright.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("cached limits appear before a slow read on open and restart", async ({
  page,
}, info) => {
  test.setTimeout(180000);
  const repo = join(import.meta.dirname, "../../..");
  const root = await mkdtemp(join(tmpdir(), "limits-cache-ui-"));
  const fixture = spawnFixture(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  const releases = [];
  let log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const initial = await readTestState(origin);
    const lead = initial.threads.find((agent) => agent.name === "Release lead");
    const now = Date.now() / 1000;
    const snapshot = (accountKey, usedPercent, at) => ({
      accountKey,
      at,
      error: null,
      data: {
        accountId: accountKey,
        rateLimits: {
          limitId: "codex",
          primary: {
            usedPercent,
            windowDurationMins: 300,
            resetsAt: now + 3600,
          },
          secondary: {
            usedPercent,
            windowDurationMins: 10080,
            resetsAt: now + 86400,
          },
        },
      },
    });
    await page.addInitScript(
      ({ scope, values }) => {
        if (!localStorage.getItem(`codex-limits:${scope}`))
          localStorage.setItem(`codex-limits:${scope}`, JSON.stringify(values));
      },
      {
        scope: initial.stateDir,
        values: {
          default: snapshot("default", 42, now - 120),
          work: snapshot("work", 80, now - 120),
        },
      },
    );
    await page.route("**/api/accounts", (route) =>
      route.fulfill({
        json: {
          defaultAccountKey: "default",
          accounts: [
            {
              id: "default",
              accountId: "default",
              label: "Personal",
              provider: "codex",
              status: "ready",
            },
            {
              id: "work",
              accountId: "work",
              label: "Work",
              provider: "codex",
              status: "ready",
            },
            {
              id: "unknown",
              accountId: "unknown",
              label: "Unknown",
              provider: "codex",
              status: "ready",
            },
          ],
        },
      }),
    );
    await page.route(/\/api\/sync\/pull\?scope=state/, async (route) => {
      const response = await route.fetch();
      const projection = await response.json();
      for (const document of projection.documents ?? []) {
        if (document.id !== "entity:workspace:current" || document._deleted)
          continue;
        const entity = JSON.parse(document.payload);
        entity.value.rateLimits = {
          accountKey: "default",
          data: null,
          at: null,
          error: null,
        };
        entity.value.rateLimitsByAccount = {};
        document.payload = JSON.stringify(entity);
      }
      await route.fulfill({ response, json: projection });
    });
    let release;
    let replies = 0;
    const gate = () =>
      new Promise((resolve) => {
        release = resolve;
        releases.push(resolve);
      });
    let pending = gate();
    await page.route(/\/api\/limits(?:\?|$)/, async (route) => {
      await pending;
      const key =
        new URL(route.request().url()).searchParams.get("account_key") ||
        "default";
      replies++;
      await route.fulfill({
        json: snapshot(key, key === "default" ? 10 : 70, now + replies),
      });
    });
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await page
      .getByRole("button", { name: "Account limits", exact: true })
      .click();
    const panel = page.getByRole("region", {
      name: "Account limits details",
      exact: true,
    });
    await expect(panel).toBeVisible();
    await page.screenshot({
      path: info.outputPath("cached-panel-before-response.png"),
    });
    await expect(panel.getByText("58%", { exact: false }).first()).toBeVisible({
      timeout: 1000,
    });
    await expect(panel.getByText(/Loading account limits/)).toHaveCount(0);
    await expect(panel.locator(".mantine-Button-loader")).toHaveCount(0);
    expect(replies).toBe(0);
    // Hold the provider response for at least three seconds after the panel opens.
    await page.waitForTimeout(3000);
    release();
    await expect(
      panel.getByText("90%", { exact: false }).first(),
    ).toBeVisible();
    await page.screenshot({ path: info.outputPath("fresh-panel.png") });
    pending = gate();
    await page.reload();
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await page
      .getByRole("button", { name: "Account limits", exact: true })
      .click();
    await expect(panel.getByText("90%", { exact: false }).first()).toBeVisible({
      timeout: 1000,
    });
    await page.screenshot({
      path: info.outputPath("cached-panel-after-restart.png"),
    });
    await page.keyboard.press("Escape");
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    await page
      .getByRole("dialog", { name: "Chat settings", exact: true })
      .getByRole("button", { name: "Main agent settings", exact: true })
      .click();
    const personal = page
      .locator('.setup-account[data-account-key="default"]')
      .first();
    const work = page
      .locator('.setup-account[data-account-key="work"]')
      .first();
    await page.screenshot({
      path: info.outputPath("account-tiles-before-response.png"),
    });
    await expect(personal.getByRole("meter")).toHaveAttribute(
      "aria-valuenow",
      "90",
      { timeout: 1000 },
    );
    await expect(work.getByRole("meter")).toHaveAttribute(
      "aria-valuenow",
      "20",
      { timeout: 1000 },
    );
    const unknown = page
      .locator('.setup-account[data-account-key="unknown"]')
      .first();
    await expect(unknown).toBeVisible();
    await expect(unknown.getByRole("meter")).toHaveCount(0);
    await page.screenshot({
      path: info.outputPath("cached-account-tiles.png"),
    });
    await page.keyboard.press("Escape");
    const chatSettings = page.getByRole("dialog", {
      name: "Chat settings",
      exact: true,
    });
    if (await chatSettings.isVisible())
      await chatSettings
        .getByRole("button", { name: "Close", exact: true })
        .click();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const studioSettings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await expect(
      studioSettings.locator('[data-account="default"]').getByRole("meter"),
    ).toHaveAttribute("aria-valuenow", "90", { timeout: 1000 });
    await expect(
      studioSettings.locator('[data-account="work"]').getByRole("meter"),
    ).toHaveAttribute("aria-valuenow", "20", { timeout: 1000 });
    await page.screenshot({
      path: info.outputPath("cached-account-cards.png"),
    });
    await studioSettings
      .getByRole("button", { name: "Close", exact: true })
      .click();
    const projectOptions = page
      .getByRole("button", { name: /^Options for project / })
      .first();
    await projectOptions.locator("..").hover();
    await projectOptions.click();
    await page
      .getByRole("menuitem", { name: "Project account", exact: true })
      .click();
    const projectSettings = page.getByRole("dialog", {
      name: "Project settings",
      exact: true,
    });
    await expect(
      projectSettings
        .locator('.setup-account[data-account-key="default"]')
        .getByRole("meter")
        .first(),
    ).toHaveAttribute("aria-valuenow", "90", { timeout: 1000 });
    await expect(
      projectSettings
        .locator('.setup-account[data-account-key="work"]')
        .getByRole("meter")
        .first(),
    ).toHaveAttribute("aria-valuenow", "20", { timeout: 1000 });
    await page.screenshot({
      path: info.outputPath("cached-project-account-tiles.png"),
    });
    release();
  } finally {
    for (const resolve of releases) resolve();
    await page.unrouteAll({ behavior: "ignoreErrors" });
    await page.close();
    fixture.kill("SIGTERM");
  }
});
