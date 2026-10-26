import type { components } from "../../src/generated/api.ts";
import type {} from "./unified-agent-picker-browser.fixture.tsx";
import { required } from "./fixture-helpers.ts";
import { fixtureUrl } from "./fixture-helpers.ts";
import {
  API_SCHEMA_HASH_HEADER,
  readApiSchemaHash,
  apiSchemaHandshakeSse,
} from "../playwright.ts";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "../playwright.ts";
import { chooseSetupValue } from "../fixtures/setup-controls.ts";

// Production components with isolated HTTP responses. No live account or model request.
test("unified agent picker roles, accounts and project/shared flows", async ({
  browser,
}) => {
  test.setTimeout(45_000);
  const root = join(import.meta.dirname, "../../../web");

  const { createFixtureViteServer: createServer } =
    await import("../playwright.ts");
  const cacheDir = await mkdtemp(join(tmpdir(), "studio-unified-picker-vite-"));
  const output =
    process.env.PICKER_SCREENSHOTS ||
    join(tmpdir(), "studio-unified-picker-shots");
  await mkdir(output, { recursive: true });
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: { host: "127.0.0.1", port: 0 },
    optimizeDeps: {
      noDiscovery: true,
      include: [
        "react",
        "react/jsx-runtime",
        "react/jsx-dev-runtime",
        "react-dom/client",
        "react-dom",
        "@mantine/core",
        "@mantine/hooks",
        "lucide-react",
        "rxdb",
        "rxdb/plugins/storage-dexie",
        "rxdb/plugins/leader-election",
        "rxdb/plugins/replication",
      ],
    },
    plugins: [
      {
        name: "unified-picker-fixture",
        configureServer(server) {
          server.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<meta name="viewport" content="width=device-width,initial-scale=1"><div id="root"></div><script type="module" src="/tests/settings/unified-agent-picker-browser.fixture.tsx"></script>',
            );
          });
        },
      },
    ],
  });
  const results: unknown[] = [];
  const contexts = [];
  try {
    await server.listen();
    const url = fixtureUrl(server, "check");
    for (const [profile, width, scheme] of [
      ["dark", 1440, "dark"],
      ["light", 1440, "light"],
      ["mobile", 390, "dark"],
    ] as const) {
      const context = await browser.newContext({
        viewport: { width, height: 900 },
        colorScheme: scheme,
      });
      contexts.push(context);
      const page = await context.newPage();
      const errors: string[] = [];
      page.on("pageerror", (error) => {
        errors.push(error.message);
        console.error(error.message);
      });
      page.on("console", (message) => {
        if (message.type() === "error") console.error(message.text());
      });
      await page.route("**/api/sync/stream?**", (route) => {
        const resources = JSON.parse(
          new URL(route.request().url()).searchParams.get("resources") || "[]",
        );
        return route.fulfill({
          contentType: "text/event-stream",
          body: apiSchemaHandshakeSse(
            `event: resources\ndata: ${JSON.stringify({ protocol: 4, workspaceId: "1234567890abcdef1234567890abcdef", epoch: "fixture", revision: 1, reason: "initial", resources })}\n\n`,
          ),
        });
      });
      await page.route("**/api/sync/identity", (route) =>
        route.fulfill({
          headers: { [API_SCHEMA_HASH_HEADER]: readApiSchemaHash() },
          json: {
            workspaceId: "1234567890abcdef1234567890abcdef",
            syncProtocol: 2,
          } satisfies components["schemas"]["SyncIdentityResponse"],
        }),
      );
      await page.goto(url);
      await page.waitForFunction(
        () => window.unifiedPickerFixture?.ready,
        null,
        { timeout: 30000 },
      );
      await page.evaluate(
        (scheme) => window.unifiedPickerFixture.scheme(scheme),
        scheme,
      );
      const trigger = page.getByRole("button", {
        name: "Main agent settings",
        exact: true,
      });
      await expect(trigger.locator(".execution-model")).toHaveText("Astra 6");
      await expect(trigger.locator(".execution-effort")).toHaveText("Medium");
      await expect(trigger.locator(".execution-account")).toHaveText("Work 1");
      assert.equal(
        await trigger
          .locator(".execution-selected")
          .evaluate((node) => node.scrollWidth <= node.clientWidth),
        true,
      );
      await page
        .getByRole("button", { name: "Main agent settings", exact: true })
        .click();
      await expect(
        page.getByRole("listbox", { name: "Main agent model" }),
      ).toBeVisible();
      const capture = async (scene: string) => {
        if (process.env.PICKER_CLIP_DETECTOR) {
          const {
            detectTextClipping,
          }: {
            detectTextClipping: (args: {
              scene: string;
              profile: string;
            }) => unknown;
          } = await import(process.env.PICKER_CLIP_DETECTOR);
          const clipping: unknown = await page.evaluate(detectTextClipping, {
            scene,
            profile,
          });
          assert.ok(Array.isArray(clipping));
          results.push(...clipping);
        }
      };
      await expect(page.locator('[role="meter"]').first()).toHaveAttribute(
        "aria-valuenow",
        "80",
      );
      await expect(
        page.locator('[data-account-key="account-3"]'),
      ).toHaveAttribute("data-exhausted", "true");
      await capture("orchestrator-codex");
      await chooseSetupValue(
        page.getByLabel("Main agent reasoning", { exact: true }),
        "high",
      );
      await page.waitForFunction(() =>
        window.unifiedPickerFixture.calls.some(
          (call) => call.effort === "high",
        ),
      );
      await page
        .getByRole("switch", { name: "Fast mode", exact: true })
        .click();
      await page.waitForFunction(() =>
        window.unifiedPickerFixture.calls.some(
          (call) => call.fast_mode === true,
        ),
      );
      await page.getByText("Worker", { exact: true }).click();
      await expect(
        page.getByRole("listbox", { name: "Default subagent model" }),
      ).toBeVisible();
      await capture("worker-codex");
      await chooseSetupValue(
        page.getByLabel("Subagent account", { exact: true }),
        "account-4",
      );
      // A new subagent account saves the defaults; nothing moves.
      await page.waitForFunction(() =>
        window.unifiedPickerFixture.calls.some(
          (call) =>
            call.endpoint === "/api/conversation" &&
            call.worker_defaults?.account_key === "account-4",
        ),
      );
      assert.equal(
        await page.evaluate(
          () =>
            window.unifiedPickerFixture.calls.filter(
              (call) => call.endpoint === "/api/agents/account-transfer",
            ).length,
        ),
        0,
      );
      await expect(
        page.getByRole("option", {
          name: "Opus 5.5 recommended",
          exact: true,
        }),
      ).toBeVisible();
      await capture("worker-claude");
      assert.equal(
        await page
          .getByRole("switch", { name: "Daybreak", exact: true })
          .count(),
        0,
      );
      assert.equal(
        await page
          .getByRole("switch", { name: "Fast mode", exact: true })
          .count(),
        0,
      );
      // Another provider's tab lists the models of all its ready accounts. A
      // model there is saved with Automatic, and the server picks the account.
      await page
        .getByLabel("Subagent account provider", { exact: true })
        .locator("label")
        .filter({ hasText: "Codex" })
        .click();
      const luna = page.getByRole("option", { name: "Luna 6", exact: true });
      await expect(luna).toBeVisible();
      const mark = await page.evaluate(
        () => window.unifiedPickerFixture.calls.length,
      );
      await luna.click();
      await page.waitForFunction(
        (mark) =>
          window.unifiedPickerFixture.calls
            .slice(mark)
            .some(
              (call) =>
                call.endpoint === "/api/conversation" &&
                call.worker_defaults?.account_key === null &&
                call.worker_defaults?.model === "gpt-6-luna",
            ),
        mark,
      );
      assert.equal(
        await page.evaluate(
          (mark) =>
            window.unifiedPickerFixture.calls
              .slice(mark)
              .filter(
                (call) => call.endpoint === "/api/agents/account-transfer",
              ).length,
          mark,
        ),
        0,
      );
      await expect(luna).toHaveAttribute("aria-selected", "true");
      await page.getByText("Review", { exact: true }).click();
      await expect(
        page.getByText("Uses the caller account.", { exact: true }),
      ).toBeVisible();
      assert.equal(
        await page.getByLabel("Subagent account", { exact: true }).count(),
        0,
      );
      assert.equal(
        await page.getByRole("option", { name: /Sonnet/ }).count(),
        0,
      );
      // Restore automatic workers, so review defaults have the Codex catalog accepted by the backend.
      await page.evaluate(() =>
        window.unifiedPickerFixture.updateAgent({
          ...window.unifiedPickerFixture.agent,
          workerDefaults: {
            model: "gpt-6-luna",
            effort: "high",
            fastMode: false,
          },
        }),
      );
      await expect(
        page.getByRole("option", { name: "Astra 6 recommended", exact: true }),
      ).toBeVisible();
      await page
        .getByRole("option", { name: "Astra 6 recommended", exact: true })
        .click();
      await page.waitForFunction(() =>
        window.unifiedPickerFixture.calls.some(
          (call) => call.review_defaults?.model === "gpt-6-astra",
        ),
      );
      await capture("review-codex");
      await page.getByText("Orchestrator", { exact: true }).click();
      await chooseSetupValue(
        page.getByLabel("Main agent account", { exact: true }),
        "account-5",
      );
      // A default alias reads as the model it resolves to, listed once.
      await expect(
        page.getByRole("option", {
          name: "Opus 5.5 recommended",
          exact: true,
        }),
      ).toBeVisible();
      await expect(
        page.getByRole("option", { name: /^Opus 5\.5/ }),
      ).toHaveCount(1);
      await expect(page.locator(".execution-menu").first()).toContainText(
        "Opus 5.5",
      );
      await expect(page.locator(".execution-menu").first()).not.toContainText(
        "default",
      );
      await capture("orchestrator-claude");
      await page.evaluate(() => {
        window.unifiedPickerFixture.reset();
        window.unifiedPickerFixture.scene("rows");
      });
      const mainRow = page.getByRole("button", {
        name: "Main agent settings",
        exact: true,
      });
      const workersRow = page.getByRole("button", {
        name: "Subagent defaults",
        exact: true,
      });
      const reviewRow = page.getByRole("button", {
        name: "Review settings",
        exact: true,
      });
      await expect(mainRow.locator(".execution-model")).toHaveText("Astra 6");
      await expect(mainRow.locator(".execution-effort")).toHaveText("Medium");
      await expect(mainRow.locator(".execution-account")).toHaveText("Work 1");
      await expect(workersRow.locator(".execution-model")).toHaveText("Luna 6");
      await expect(workersRow.locator(".execution-effort")).toHaveText("High");
      await expect(workersRow.locator(".execution-account")).toHaveText(
        "Work 1",
      );
      await expect(reviewRow.locator(".execution-model")).toHaveText(
        "Same as caller",
      );
      await expect(reviewRow.locator(".execution-effort")).toHaveCount(0);
      await expect(reviewRow.locator(".execution-selected > svg")).toHaveCount(
        0,
      );
      await expect(reviewRow).not.toContainText("Work 1");
      await expect(reviewRow.locator(".execution-selected")).toHaveAttribute(
        "data-inherited",
        "true",
      );
      for (const [row, role] of [
        [mainRow, "Orchestrator"],
        [workersRow, "Worker"],
        [reviewRow, "Review"],
      ] as const) {
        await required(row).click();
        await expect(
          page.getByRole("radio", { name: role, exact: true }),
        ).toBeChecked();
        await page.keyboard.press("Escape");
        await expect(page.locator(".execution-dropdown:visible")).toHaveCount(
          0,
        );
      }
      await reviewRow.click();
      await page
        .getByLabel("Default review model", { exact: true })
        .locator('[data-value="gpt-6-luna"]')
        .click();
      await chooseSetupValue(
        page.getByLabel("Default review reasoning", { exact: true }),
        "high",
      );
      await page.keyboard.press("Escape");
      await expect(reviewRow.locator(".execution-model")).toHaveText("Luna 6");
      await expect(reviewRow.locator(".execution-effort")).toHaveText("High");
      await expect(
        reviewRow.locator(".execution-selected"),
      ).not.toHaveAttribute("data-inherited");
      assert.equal(
        await page.evaluate(
          () =>
            window.unifiedPickerFixture.required(
              window.unifiedPickerFixture.calls
                .filter((call) => call.review_defaults)
                .at(-1),
            ).expected_account_key,
        ),
        "account-0",
      );
      await capture("chat-settings-model-rows");
      for (const scene of ["chat-account", "project", "shared"] as const) {
        await page.evaluate(
          (scene) => window.unifiedPickerFixture.scene(scene),
          scene,
        );
        if (scene === "shared")
          await page
            .getByRole("button", { name: "Settings for agent 1" })
            .click();
        const group = page.getByLabel(
          scene === "chat-account"
            ? "Account"
            : scene === "project"
              ? "Accounts shown first for this project"
              : "Account for agent 1",
          { exact: true },
        );
        await expect(group).toBeVisible();
        await capture(scene + "-codex");
        await group.locator("label").filter({ hasText: "Claude" }).click();
        await capture(scene + "-claude");
        if (scene === "project") {
          await expect(
            group.locator('[data-account-key="account-6"]'),
          ).toHaveAttribute("data-exhausted", "true");
          await group.locator('[data-account-key="account-4"]').click();
          await page
            .getByRole("button", { name: "Save accounts", exact: true })
            .click();
          await page.waitForFunction(() =>
            window.unifiedPickerFixture.calls.some(
              (call) =>
                call.endpoint === "/api/projects" &&
                !window.unifiedPickerFixture
                  .required(call.account_keys)
                  .includes("account-4"),
            ),
          );
        } else if (scene === "shared") {
          await group.locator('[data-account-key="account-5"]').click();
          await expect(
            page
              .getByLabel("Model for agent 1", { exact: true })
              .getByRole("option", { selected: true }),
          ).toHaveAttribute("data-value", "default");
          await chooseSetupValue(
            page.getByLabel("Reasoning for agent 1", { exact: true }),
            "high",
          );
          await page.keyboard.press("Escape");
          await page
            .getByRole("button", { name: "Create", exact: true })
            .click();
          await page.waitForFunction(() =>
            window.unifiedPickerFixture.calls.some(
              (call) => call.endpoint === "/api/peer-teams",
            ),
          );
          assert.deepEqual(
            await page.evaluate(() => {
              const request = window.unifiedPickerFixture.required(
                window.unifiedPickerFixture.calls.find(
                  (call) => call.endpoint === "/api/peer-teams",
                ),
              );
              const participant = window.unifiedPickerFixture.required(
                request.participants,
              )[0];
              return [
                window.unifiedPickerFixture.required(participant).account_key,
                window.unifiedPickerFixture.required(participant).model,
                window.unifiedPickerFixture.required(participant).effort,
              ];
            }),
            ["account-5", "default", "high"],
          );
        }
      }
      await page.evaluate(() => {
        window.unifiedPickerFixture.reset();
        window.unifiedPickerFixture.whitelist([
          "codex:gpt-6-astra",
          "claude:claude-sonnet-5-5",
        ]);
      });
      await page
        .getByRole("button", { name: "Main agent settings", exact: true })
        .click();
      await page.getByText("Worker", { exact: true }).click();
      await chooseSetupValue(
        page.getByLabel("Subagent account", { exact: true }),
        "account-4",
      );
      await expect
        .poll(() =>
          page.evaluate(
            () =>
              window.unifiedPickerFixture.required(
                window.unifiedPickerFixture.agent.workerDefaults,
              ).model,
          ),
        )
        .toBe("claude-sonnet-5-5");
      await expect(page.getByRole("option", { name: /Opus/ })).toHaveCount(0);
      await page.evaluate(() => window.unifiedPickerFixture.whitelist(null));
      assert.deepEqual(errors, []);
    }
    await writeFile(
      join(output, "clipping.json"),
      JSON.stringify(results, null, 2),
    );
    console.log(`Clipping measurements: ${output}`);
    assert.deepEqual(
      results.filter(
        (row) =>
          !(
            row &&
            typeof row === "object" &&
            "intentional" in row &&
            row.intentional
          ),
      ),
      [],
    );
  } finally {
    await Promise.all(contexts.map((context) => context.close()));
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
