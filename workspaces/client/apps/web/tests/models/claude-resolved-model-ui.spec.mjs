import { mkdtemp, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  readTestState,
  spawnFixture,
  rustUiFixture,
  test,
  expect,
} from "../playwright.mjs";
import { waitForFixturePort } from "../fixture-readiness.mjs";

// Run the production renderer against disposable runtime state. Claude account
// and catalog projections reproduce the real CLI's stored resolved model ID.
test("Claude resolved model ids select their available catalog alias", async ({
  page,
}) => {
  test.setTimeout(90000);
  const root = await mkdtemp(join(tmpdir(), "claude-resolved-model-"));
  const server = spawnFixture(process.execPath, [rustUiFixture, root], {
    env: { ...process.env, FIXTURE_WORKER_COUNT: "0" },
    stdio: ["pipe", "pipe", "pipe"],
  });
  let log = "";
  server.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await waitForFixturePort(server, { diagnostics: () => log });
    const origin = `http://127.0.0.1:${port}`;
    const lead = (await readTestState(origin)).runtime.agents.find(
      (agent) => agent.name === "Release lead",
    );
    const models = [
      {
        model: "default",
        displayName: "Claude · Default (recommended)",
        isDefault: true,
        resolvedModel: "claude-opus-5-5",
      },
      {
        model: "opus",
        displayName: "Claude · Opus 5.5",
        resolvedModel: "claude-opus-5-5",
      },
      {
        model: "sonnet",
        displayName: "Claude · Sonnet 5.5",
        resolvedModel: "claude-sonnet-5-5",
      },
    ].map((row) => ({
      ...row,
      provider: "claude",
      description: "Claude coding model",
      defaultReasoningEffort: "medium",
      supportedReasoningEfforts: [
        { reasoningEffort: "low" },
        { reasoningEffort: "medium" },
        { reasoningEffort: "high" },
      ],
      serviceTiers: [],
    }));
    await page.route("**/api/accounts", async (route) => {
      const response = await route.fetch();
      const data = await response.json();
      data.accounts = data.accounts.map((account) => ({
        ...account,
        provider: "claude",
        label: "Claude work",
        status: "ready",
      }));
      await route.fulfill({ response, json: data });
    });
    await page.route("**/api/models?**", (route) =>
      route.fulfill({ json: { data: models } }),
    );
    await page.route("**/api/claude/session", (route) =>
      route.fulfill({
        json: {
          settings: { permissionMode: "acceptEdits", thinking: true },
          turns: [],
        },
      }),
    );
    await page.route("**/api/sync/pull*", async (route) => {
      const response = await route.fetch();
      const data = await response.json();
      for (const document of data.documents || []) {
        if (document._deleted) continue;
        const entity = JSON.parse(document.payload);
        if (entity.collection === "agent" && entity.id === lead.id) {
          Object.assign(entity.value, {
            provider: "claude",
            model: "claude-opus-5-5",
            effort: "medium",
            workerDefaults: {
              accountKey: "default",
              model: "claude-opus-5-5",
              effort: "medium",
            },
          });
          document.payload = JSON.stringify(entity);
        }
      }
      await route.fulfill({ response, json: data });
    });
    await page.goto(origin);
    await page.locator(`[data-chat="${lead.id}"]`).click();
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    const dialog = page.getByRole("dialog", {
      name: "Chat settings",
      exact: true,
    });
    const reviewSummary = dialog.locator(
      'details[data-role="review"] > summary',
    );
    await expect(reviewSummary.locator(".execution-model")).toHaveText(
      "Same as caller",
    );
    await expect(reviewSummary.locator(".execution-effort")).toHaveCount(0);
    await expect(
      reviewSummary.locator(".execution-selected > svg"),
    ).toHaveCount(0);
    for (const role of ["orchestrator", "worker"]) {
      const section = dialog.locator(`details[data-role="${role}"]`);
      await section.locator(":scope > summary").click();
      const selected = section.locator(
        'input[type="radio"][data-value="opus"]',
      );
      await expect(selected).toBeChecked();
      await expect(selected).toBeEnabled();
      await expect(
        section.getByText("Unavailable on this account.", { exact: true }),
      ).toHaveCount(0);
      await expect(
        section.locator('.model-options input[data-value="claude-opus-5-5"]'),
      ).toHaveCount(0);
      await expect(
        section.locator(":scope > summary .execution-model"),
      ).toHaveText("Opus 5.5");
      if (process.env.CLAUDE_RESOLVED_SCREENSHOTS) {
        await mkdir(process.env.CLAUDE_RESOLVED_SCREENSHOTS, {
          recursive: true,
        });
        await page.screenshot({
          path: join(
            process.env.CLAUDE_RESOLVED_SCREENSHOTS,
            `claude-resolved-${role}.png`,
          ),
        });
      }
    }
    await expect(
      dialog.locator('details[data-role="review"] input[data-value="opus"]'),
    ).toHaveCount(0);
  } finally {
    server.stdin?.end();
  }
});
