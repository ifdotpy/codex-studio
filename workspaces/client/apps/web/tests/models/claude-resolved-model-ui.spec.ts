import { fixtureGetResult } from "../misc-fixture.ts";
import type { GetResult } from "../../src/api.ts";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  readTestState,
  spawnFixture,
  rustUiFixture,
  test,
  expect,
} from "../playwright.ts";
import { waitForFixturePort } from "../fixture-readiness.ts";

// Run the production renderer against disposable runtime state. Claude account
// and catalog projections reproduce the real CLI's stored resolved model ID.
for (const provider of ["claude", "codex"] as const)
  test(`${provider} resolved models and compact Chat settings tabs`, async ({
    page,
  }) => {
    test.setTimeout(90000);
    const root = await mkdtemp(join(tmpdir(), "claude-resolved-model-"));
    const server = spawnFixture(process.execPath, [rustUiFixture, root], {
      env: { ...process.env, FIXTURE_WORKER_COUNT: "0" },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let log = "";
    server.stderr?.on("data", (chunk) => {
      log += chunk;
    });
    try {
      const port = await waitForFixturePort(server, { diagnostics: () => log });
      const origin = `http://127.0.0.1:${port}`;
      const lead = (await readTestState(origin)).runtime.agents.find(
        (agent) => agent.name === "Release lead",
      );
      if (!lead) throw new Error("Missing release lead");
      const currentModel =
        provider === "claude" ? "claude-opus-5-5" : "gpt-6-astra";
      const selectedAlias = provider === "claude" ? "opus" : currentModel;
      const families =
        provider === "claude"
          ? [
              "Opus 5.5",
              "Sonnet 5.5",
              "Haiku 4.5",
              "Opus 4.6",
              "Sonnet 4.6",
              "Opus 4.5",
              "Sonnet 4.5",
              "Haiku 3.5",
              "Opus 4.1",
              "Sonnet 4",
              "Opus 5.5 (1M context)",
              "Sonnet 5.5 with 1M context",
              "Sonnet 4.5 1M",
            ]
          : [
              "Astra 6",
              "Sol 6.1",
              "Luna 6",
              "Sol 5.6",
              "Luna 5.6",
              "Terra 5.6",
              "Sol 6",
              "Terra 6",
              "Astra 5.6",
              "Mini 6",
              "Mini 5.6",
              "Nano 6",
              "Nano 5.6",
            ];
      const choices = families.map((label, index) => ({
        model: index ? `${provider}-catalog-${index}` : selectedAlias,
        displayName: label,
        provider,
        resolvedModel: index ? `${provider}-resolved-${index}` : currentModel,
      }));
      const models = [
        {
          model: "default",
          displayName: "Default (recommended)",
          isDefault: true,
          provider,
          resolvedModel: currentModel,
        },
        ...choices,
      ].map((row) => ({
        ...row,
        description: "Coding and reasoning for long running project work.",
        defaultReasoningEffort: "medium",
        supportedReasoningEfforts: ["low", "medium", "high"].map(
          (reasoningEffort) => ({ reasoningEffort }),
        ),
        serviceTiers: [],
      }));
      const catalog = { data: models } satisfies GetResult<"/api/models">;
      const screenshots = process.env.SETTINGS_TABS_SCREENSHOTS
        ? join(process.env.SETTINGS_TABS_SCREENSHOTS, provider)
        : await mkdtemp(join(tmpdir(), `settings-tabs-${provider}-`));
      await mkdir(screenshots, { recursive: true });
      const measurements = [];
      await page.route("**/api/accounts", async (route) => {
        const response = await route.fetch();
        const data = fixtureGetResult<"/api/accounts">(await response.json());
        for (const account of data.accounts)
          Object.assign(account, {
            ...account,
            provider,
            label: `${provider} work`,
            status: "ready",
          });
        await route.fulfill({ response, json: data });
      });
      await page.route("**/api/limits?**", (route) =>
        route.fulfill({
          json: {
            accountKey: new URL(route.request().url()).searchParams.get(
              "account_key",
            ),
            at: Date.now() / 1000,
            data: {
              rateLimits: {
                limitId: provider,
                primary: {
                  usedPercent: 9,
                  windowDurationMins: 300,
                  resetsAt: Date.now() / 1000 + 3600,
                },
              },
            },
          },
        }),
      );
      await page.route("**/api/models?**", (route) =>
        route.fulfill({ json: catalog }),
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
        const data = fixtureGetResult<"/api/sync/pull">(await response.json());
        for (const document of "documents" in data ? data.documents : []) {
          if (!("payload" in document))
            throw new Error("Fixture expects legacy sync payloads");
          if (document._deleted) continue;
          const entity = JSON.parse(document.payload);
          if (entity.collection === "agent" && entity.id === lead.id) {
            Object.assign(entity.value, {
              provider,
              model: currentModel,
              effort: "medium",
              workerDefaults: {
                accountKey: "default",
                model: currentModel,
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
      const roles = dialog.getByRole("radiogroup", {
        name: "Agent role",
        exact: true,
      });
      // Main's picker presents a single scrollable list, keeping full names.
      await expect(
        dialog.getByRole("listbox", { name: "Main agent model", exact: true }),
      ).toBeVisible();
      if (provider === "claude") {
        await expect(
          dialog.getByRole("option", { name: /Opus/ }).first(),
        ).toBeVisible();
        await expect(
          dialog.locator(".setup-model-name").filter({ hasText: "1M" }),
        ).toHaveCount(3);
        await expect(dialog.getByRole("tab")).toHaveCount(2);
      }
      await expect(dialog.locator(".setup-account-status")).toHaveCount(1);
      await expect(dialog.locator(".setup-account-status")).not.toHaveAttribute(
        "role",
        "button",
      );
      await expect(roles.locator(".settings-role-model")).toHaveCount(0);
      await expect(
        page.locator("#composer .execution-summary-trigger"),
      ).toHaveAttribute("title", /Review same as caller/);
      for (const role of ["Lead", "Workers"]) {
        await roles.getByText(role, { exact: true }).click();
        const selected = dialog.locator(
          `.setup-models [role="option"][data-value="${selectedAlias}"]`,
        );
        await expect(selected).toHaveAttribute("aria-selected", "true");
        await expect(selected).toBeEnabled();
        await expect(
          dialog.getByText("Unavailable on this account.", { exact: true }),
        ).toHaveCount(0);
        await expect(dialog.locator(".setup-models [role=option]")).toHaveCount(
          role === "Lead" ? 13 : 14,
        );
      }
      await roles.getByText("Review", { exact: true }).click();
      await expect(
        dialog.getByRole("option", { name: "Same as caller", exact: true }),
      ).toHaveAttribute("aria-selected", "true");
      if (provider === "claude")
        await expect(
          dialog.locator('[role="option"][data-value="opus"]'),
        ).toHaveCount(0);
      await roles.getByText("Lead", { exact: true }).click();
      for (const [width, height] of [
        [1440, 900],
        [390, 844],
      ] as const)
        for (const scheme of ["light", "dark"] as const) {
          await page.setViewportSize({ width, height });
          await page.emulateMedia({ colorScheme: scheme });
          await expect(page.locator("html")).toHaveAttribute(
            "data-mantine-color-scheme",
            scheme,
          );
          for (const tab of ["Models", "Team"]) {
            await dialog
              .getByRole("tab", { name: new RegExp("^" + tab) })
              .click();
            if (tab === "Models")
              await expect
                .poll(async () =>
                  roles.evaluate((node) => {
                    const indicatorElement = node.querySelector(
                      ".mantine-SegmentedControl-indicator",
                    );
                    if (!indicatorElement)
                      throw new Error("Missing settings indicator");
                    const indicator = indicatorElement.getBoundingClientRect();
                    const checkedElement = node.querySelector(
                      ".mantine-SegmentedControl-control:has(input:checked) .mantine-SegmentedControl-label",
                    );
                    if (!checkedElement)
                      throw new Error("Missing checked settings control");
                    const checked = checkedElement.getBoundingClientRect();
                    return Math.max(
                      ...(["x", "y", "width", "height"] as const).map((key) =>
                        Math.abs(indicator[key] - checked[key]),
                      ),
                    );
                  }),
                )
                .toBeLessThan(2);
            const geometry = await dialog.evaluate((node) => {
              const body = node.querySelector(".mantine-Modal-body");
              if (!body) throw new Error("Missing settings body");
              const tabs = [...node.querySelectorAll('[role="tab"]')].map(
                (el) => el.getBoundingClientRect().top,
              );
              const boxes = [
                ...node.querySelectorAll(".setup-models [role=option]"),
              ]
                .map((el) => el.getBoundingClientRect())
                .filter((box) => box.height);
              return {
                roleHeight:
                  node
                    .querySelector(".settings-role-switch")
                    ?.getBoundingClientRect().height || 0,
                listHeight:
                  node.querySelector(".setup-models")?.clientHeight || 0,
                listContent:
                  node.querySelector(".setup-models")?.scrollHeight || 0,
                dialogContent: node.scrollHeight,
                dialogHeight: node.clientHeight,
                bodyContent: body.scrollHeight,
                bodyHeight: body.clientHeight,
                tabRows: new Set(tabs).size,
                modelRows: new Set(boxes.map((box) => Math.round(box.top)))
                  .size,
                chipWidthRange: boxes.length
                  ? Math.max(...boxes.map((box) => box.width)) -
                    Math.min(...boxes.map((box) => box.width))
                  : 0,
                pageWidth: document.documentElement.scrollWidth,
                viewport: innerWidth,
              };
            });
            expect(
              geometry.dialogContent,
              JSON.stringify({ provider, width, scheme, tab, ...geometry }),
            ).toBeLessThanOrEqual(geometry.dialogHeight);
            if (width === 1440)
              expect(geometry.bodyContent).toBeLessThanOrEqual(
                geometry.bodyHeight,
              );
            expect(geometry.tabRows).toBe(1);
            expect(geometry.pageWidth).toBeLessThanOrEqual(geometry.viewport);
            if (tab === "Models") {
              expect(geometry.modelRows).toBe(13);
              expect(geometry.listHeight).toBeLessThan(geometry.listContent);
            }
            expect(geometry.chipWidthRange).toBeLessThan(2);
            if (tab === "Models")
              expect(geometry.roleHeight).toBeLessThanOrEqual(44);
            measurements.push({
              provider,
              width,
              height,
              scheme,
              tab,
              ...geometry,
            });
            await page.screenshot({
              path: join(screenshots, `${tab}-${width}-${scheme}.png`),
            });
            if (tab === "Models") {
              await roles.getByText("Workers", { exact: true }).click();
              await expect
                .poll(async () =>
                  roles.evaluate((node) => {
                    const indicatorElement = node.querySelector(
                      ".mantine-SegmentedControl-indicator",
                    );
                    if (!indicatorElement)
                      throw new Error("Missing settings indicator");
                    const indicator = indicatorElement.getBoundingClientRect();
                    const checkedElement = node.querySelector(
                      ".mantine-SegmentedControl-control:has(input:checked) .mantine-SegmentedControl-label",
                    );
                    if (!checkedElement)
                      throw new Error("Missing checked settings control");
                    const checked = checkedElement.getBoundingClientRect();
                    return Math.max(
                      ...(["x", "y", "width", "height"] as const).map((key) =>
                        Math.abs(indicator[key] - checked[key]),
                      ),
                    );
                  }),
                )
                .toBeLessThan(2);
              await expect(
                dialog.getByRole("radio", { name: "Automatic", exact: true }),
              ).not.toBeChecked();
              await page.screenshot({
                path: join(screenshots, `Workers-${width}-${scheme}.png`),
              });
              await roles.getByText("Lead", { exact: true }).click();
            }
          }
        }
      await writeFile(
        join(screenshots, "measurements.json"),
        JSON.stringify(measurements, null, 2),
      );
      console.log(
        `Settings tabs ${provider} screenshots and measurements: ${screenshots}`,
      );
    } finally {
      server.stdin?.end();
    }
  });
