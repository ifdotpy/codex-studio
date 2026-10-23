import { test, expect, spawnFixture, rustUiFixture } from "../playwright.mjs";
import { waitForFixturePort } from "../fixture-readiness.mjs";
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
  fixture.stderr.on("data", (chunk) => {
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
      .locator("#composer .execution-selected")
      .innerText();
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
    await whitelist
      .getByRole("checkbox", { name: "Luna 6", exact: true })
      .uncheck();
    for (const width of [320, 390, 1440])
      for (const scheme of ["light", "dark"]) {
        await page.setViewportSize({ width, height: 900 });
        await page.emulateMedia({ colorScheme: scheme });
        await page.evaluate((scheme) => {
          document.documentElement.dataset.mantineColorScheme = scheme;
        }, scheme);
        await expect(whitelist).toBeVisible();
        expect(
          await settings.evaluate((el) => el.scrollWidth <= el.clientWidth),
        ).toBe(true);
        await page.screenshot({
          path: join(evidence, `models-${width}-${scheme}.png`),
        });
      }
    await page.keyboard.press("Escape");
    await expect(settings).toBeHidden();
    await expect(page.locator("#composer .execution-selected")).toHaveText(
      current,
    );
    await page
      .getByRole("button", { name: "Main agent settings", exact: true })
      .click();
    const picker = page.getByRole("dialog", {
      name: "Main agent settings",
      exact: true,
    });
    await expect(
      picker
        .getByLabel("Main agent model", { exact: true })
        .getByRole("radio", { name: /Luna/ }),
    ).toHaveCount(0);
    await picker.getByText("Worker", { exact: true }).click();
    const worker = page.getByRole("dialog", {
      name: "Subagent defaults",
      exact: true,
    });
    const workerModel = worker.getByLabel("Default subagent model", {
      exact: true,
    });
    // An existing worker default remains visible, but cannot be selected anew.
    await expect(
      workerModel.getByRole("radio", { name: /Luna/ }),
    ).toBeChecked();
    await expect(
      workerModel.getByRole("radio", { name: /Luna/ }),
    ).toBeDisabled();
    await worker.getByText("Review", { exact: true }).click();
    await expect(
      page
        .getByLabel("Default review model", { exact: true })
        .getByRole("radio", { name: /Luna/ }),
    ).toHaveCount(0);
    await page.getByText("Orchestrator", { exact: true }).click();
    await page.keyboard.press("Escape");
    const openNewChat = async () => {
      await page
        .locator(".sidebar-nav")
        .getByRole("button", { name: "New chat", exact: true })
        .click();
      return page.getByRole("dialog", { name: "New chat", exact: true });
    };
    const creation = await openNewChat();
    await expect(
      creation
        .getByLabel("Main agent model", { exact: true })
        .getByRole("radio", { name: /Luna/ }),
    ).toHaveCount(0);
    await expect(
      creation
        .getByLabel("Default subagent model", { exact: true })
        .getByRole("radio", { name: /Luna/ }),
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
      .getByRole("button", { name: "Clear selection", exact: true })
      .click();
    await page.keyboard.press("Escape");
    await page
      .getByRole("button", { name: "Main agent settings", exact: true })
      .click();
    const retained = page.getByLabel("Main agent model", { exact: true });
    await expect(retained.getByRole("radio")).toHaveCount(1);
    await expect(retained.getByRole("radio")).toBeChecked();
    await expect(retained.getByRole("radio")).toBeDisabled();
    await expect(page.locator("#composer .execution-selected")).toHaveText(
      current,
    );
    await page.keyboard.press("Escape");
    await openNewChat();
    await expect(
      creation.getByRole("button", { name: "Start chat", exact: true }),
    ).toBeDisabled();
    await expect(creation.getByRole("alert")).toContainText(
      "No enabled models",
    );
    await creation.getByRole("button", { name: "Cancel", exact: true }).click();
    // Persistence is pending: the current experimental owner deliberately resets.
    await page.reload();
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    await settings.getByRole("tab", { name: "Models", exact: true }).click();
    await expect(whitelist.getByRole("checkbox")).toHaveCount(3);
    for (const checkbox of await whitelist.getByRole("checkbox").all())
      await expect(checkbox).toBeChecked();
  } finally {
    fixture.kill("SIGTERM");
  }
});
