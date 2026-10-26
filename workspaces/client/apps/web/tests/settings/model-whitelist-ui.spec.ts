import type { PostBody } from "../../src/apiTypes.ts";
import type { Route } from "@playwright/test";
import { test, expect, spawnFixture, rustUiFixture } from "../playwright.ts";
import { waitForFixturePort } from "../fixture-readiness.ts";
import { mkdtemp, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const names = ["gpt-6-astra", "gpt-6-luna", "gpt-5.6-sol"];
const models = names.map((model, index) => ({
  model,
  provider: "codex",
  isDefault: index === 0,
  defaultReasoningEffort: "medium",
  supportedReasoningEfforts: [
    { reasoningEffort: "medium" },
    { reasoningEffort: "high" },
  ],
  serviceTiers: [],
}));

test("Models whitelist filters every execution role and preserves the current model", async ({
  page,
  browser,
}) => {
  test.setTimeout(90000);
  const root = await mkdtemp(join(tmpdir(), "studio-model-whitelist-"));
  const evidence = process.env.MODELS_EVIDENCE || root;
  await mkdir(evidence, { recursive: true });
  const fixture = spawnFixture(process.execPath, [rustUiFixture, root], {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      EXECUTION_SETTINGS_CATALOG: JSON.stringify(models),
      FIXTURE_HOLD: "lead",
    },
  });
  let log = "";
  fixture.stderr?.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await waitForFixturePort(fixture, { diagnostics: () => log });
    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(`http://127.0.0.1:${port}`);
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .first()
      .click();
    const current = await page
      .locator("#composer .execution-summary-trigger")
      .getAttribute("title");
    if (current === null) throw new Error("Missing current model title");
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const settings = page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
    await settings.getByRole("tab", { name: "Models", exact: true }).click();
    const whitelist = settings.getByRole("tabpanel", {
      name: "Models",
      exact: true,
    });
    await expect(whitelist.getByRole("checkbox")).toHaveCount(3);
    for (const checkbox of await whitelist.getByRole("checkbox").all())
      await expect(checkbox).toBeChecked();
    const failPreferenceWrite = async (route: Route) => {
      const body: PostBody<"/api/sync/preferences"> = route
        .request()
        .postDataJSON();
      if (body.scope === "server") await route.abort("failed");
      else await route.continue();
    };
    await page.route("**/api/sync/preferences", failPreferenceWrite);
    await whitelist
      .getByRole("checkbox", { name: "Luna 6", exact: true })
      .uncheck();
    await expect(
      whitelist.getByText("Model selection is not saved.", { exact: true }),
    ).toBeVisible();
    await expect(whitelist.getByRole("alert")).toBeVisible();
    await page.unroute("**/api/sync/preferences", failPreferenceWrite);
    await whitelist
      .getByRole("button", { name: "Retry saving", exact: true })
      .click();
    await expect(
      whitelist.getByText("Model selection saved on this server.", {
        exact: true,
      }),
    ).toBeVisible();
    for (const width of [320, 390, 1440])
      for (const scheme of ["light", "dark"] as const) {
        await page.setViewportSize({ width, height: 900 });
        await page.emulateMedia({ colorScheme: scheme });
        await page.evaluate((scheme) => {
          document.documentElement.dataset.mantineColorScheme = scheme;
        }, scheme);
        await expect(whitelist).toBeVisible();
        expect(
          await settings.evaluate((el) => el.scrollWidth <= el.clientWidth),
        ).toBe(true);
        const columns = await whitelist
          .locator(".model-whitelist-grid")
          .evaluate(
            (el) => getComputedStyle(el).gridTemplateColumns.split(" ").length,
          );
        if (width === 1440) expect(columns).toBeGreaterThanOrEqual(2);
        else expect(columns).toBe(1);
        expect(
          await whitelist.evaluate(
            (el) =>
              Math.abs(
                el.getBoundingClientRect().width -
                  (el.parentElement?.clientWidth ?? 0),
              ) < 16,
          ),
        ).toBe(true);
        await page.screenshot({
          path: join(evidence, `models-${width}-${scheme}.png`),
        });
      }
    await page.keyboard.press("Escape");
    await expect(settings).toBeHidden();
    await expect(
      page.locator("#composer .execution-summary-trigger"),
    ).toHaveAttribute("title", current);
    await page.locator("#composer .execution-summary-trigger").click();
    const picker = page.locator(".execution-dropdown");
    await expect(
      picker
        .getByLabel("Main agent model", { exact: true })
        .getByRole("option", { name: /Luna/ }),
    ).toHaveCount(0);
    await picker.getByText("Workers", { exact: true }).click();
    const worker = page.locator(".execution-dropdown");
    const workerModel = worker.getByLabel("Default subagent model", {
      exact: true,
    });
    // An existing worker default remains visible, but cannot be selected anew.
    await expect(
      workerModel.getByRole("option", { name: /Luna/ }),
    ).toHaveAttribute("aria-selected", "true");
    await expect(
      workerModel.getByRole("option", { name: /Luna/ }),
    ).toBeDisabled();
    await worker.getByText("Review", { exact: true }).click();
    await expect(
      page
        .getByLabel("Default review model", { exact: true })
        .getByRole("option", { name: /Luna/ }),
    ).toHaveCount(0);
    await picker.getByText("Lead", { exact: true }).click();
    await page.keyboard.press("Escape");
    const openNewChat = async () => {
      await page
        .locator("#sidebar .projects-heading")
        .getByRole("button", { name: "New chat", exact: true })
        .click();
      return page.getByRole("dialog", { name: "New chat", exact: true });
    };
    const creation = await openNewChat();
    await expect(
      creation
        .getByLabel("Main agent model", { exact: true })
        .getByRole("option", { name: /Luna/ }),
    ).toHaveCount(0);
    await creation
      .getByRole("radiogroup", { name: "Agent role", exact: true })
      .getByText("Workers", { exact: true })
      .click();
    await expect(
      creation.getByLabel("Default subagent model", { exact: true }),
    ).toBeVisible();
    await expect(
      creation
        .getByLabel("Default subagent model", { exact: true })
        .getByRole("option", { name: /Luna/ }),
    ).toHaveCount(0);
    await expect(
      creation.getByRole("button", { name: "Start chat", exact: true }),
    ).toBeEnabled();
    await creation.getByRole("button", { name: "Cancel", exact: true }).click();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await settings.getByRole("tab", { name: "Models", exact: true }).click();
    await whitelist
      .getByRole("checkbox", { name: "Sol 5.6", exact: true })
      .uncheck();
    await page.keyboard.press("Escape");
    await page
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await page.locator('[data-workspace-section="changes"]').click();
    await page
      .getByRole("navigation", { name: "Workspace sections" })
      .getByRole("button", { name: "Profiles", exact: true })
      .click();
    await page
      .getByRole("button", { name: "New profile", exact: true })
      .click();
    const profile = page.getByRole("dialog", {
      name: "New subagent profile",
      exact: true,
    });
    await expect(profile.getByLabel("Model", { exact: true })).toHaveValue(
      "gpt-6-astra",
    );
    await expect(
      profile
        .getByLabel("Model", { exact: true })
        .locator('option[value="gpt-5.6-sol"]'),
    ).toHaveCount(0);
    await profile
      .getByRole("textbox", { name: /^Name/ })
      .fill("Enabled model profile");
    const savedProfile = page.waitForResponse(
      (response) =>
        response.url().endsWith("/api/profiles") &&
        response.request().method() === "POST",
    );
    await profile
      .getByRole("button", { name: "Save profile", exact: true })
      .click();
    const savedBody: PostBody<"/api/conversation"> = (await savedProfile)
      .request()
      .postDataJSON();
    expect(savedBody.model).toBe("gpt-6-astra");
    await page.locator(".workspace-drawer .mantine-Drawer-close").click();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await settings.getByRole("tab", { name: "Models", exact: true }).click();
    await whitelist
      .getByRole("button", { name: "Clear selection", exact: true })
      .click();
    await expect(
      whitelist.getByText("Model selection saved on this server.", {
        exact: true,
      }),
    ).toBeVisible();
    await page.keyboard.press("Escape");
    await page.locator("#composer .execution-summary-trigger").click();
    const retained = page.getByLabel("Main agent model", { exact: true });
    await expect(retained.getByRole("option")).toHaveCount(1);
    await expect(retained.getByRole("option")).toHaveAttribute(
      "aria-selected",
      "true",
    );
    await expect(retained.getByRole("option")).toBeDisabled();
    await expect(
      page.locator("#composer .execution-summary-trigger"),
    ).toHaveAttribute("title", current);
    await page.keyboard.press("Escape");
    await openNewChat();
    await expect(
      creation.getByRole("button", { name: "Start chat", exact: true }),
    ).toBeDisabled();
    await expect(creation.getByRole("alert")).toContainText(
      "No enabled models",
    );
    await creation.getByRole("button", { name: "Cancel", exact: true }).click();
    // Reload must keep the committed choice; a fresh browser has no cache or outbox.
    await page.reload();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await settings.getByRole("tab", { name: "Models", exact: true }).click();
    await expect(whitelist.getByRole("checkbox")).toHaveCount(3);
    for (const checkbox of await whitelist.getByRole("checkbox").all())
      await expect(checkbox).not.toBeChecked();
    const fresh = await browser.newContext({
      viewport: { width: 390, height: 900 },
    });
    try {
      const phone = await fresh.newPage();
      await phone.goto(`http://127.0.0.1:${port}`);
      await phone
        .getByRole("button", { name: "Studio settings", exact: true })
        .click();
      const phoneSettings = phone.getByRole("dialog", {
        name: "Studio settings",
        exact: true,
      });
      await phoneSettings
        .getByRole("tab", { name: "Models", exact: true })
        .click();
      const phoneModels = phoneSettings.getByRole("tabpanel", {
        name: "Models",
        exact: true,
      });
      await expect(phoneModels.getByRole("checkbox")).toHaveCount(3);
      for (const checkbox of await phoneModels.getByRole("checkbox").all())
        await expect(checkbox).not.toBeChecked();
      await phoneModels
        .getByRole("button", { name: "Select all models", exact: true })
        .click();
      await expect(
        phoneModels.getByText("Model selection saved on this server.", {
          exact: true,
        }),
      ).toBeVisible();
      // The first browser receives the changed server field through SSE.
      for (const checkbox of await whitelist.getByRole("checkbox").all())
        await expect(checkbox).toBeChecked();
      await phone.reload();
      await phone
        .getByRole("button", { name: "Studio settings", exact: true })
        .click();
      await phoneSettings
        .getByRole("tab", { name: "Models", exact: true })
        .click();
      await expect(
        phoneModels.getByText("All models selected, including new models.", {
          exact: true,
        }),
      ).toBeVisible();
    } finally {
      await fresh.close();
    }
  } finally {
    fixture.kill("SIGTERM");
  }
});
