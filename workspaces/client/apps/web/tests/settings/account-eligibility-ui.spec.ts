import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  readTestState,
} from "../playwright.ts";
import { waitForFixturePort } from "../fixture-readiness.ts";

for (const provider of ["codex", "claude"])
  test(`${provider} Team eligibility keeps five long accounts within provider rows`, async ({
    page,
  }) => {
    const state = await mkdtemp(join(tmpdir(), "eligibility-rows-"));
    const evidence = join(
      process.env.ACCOUNT_ELIGIBILITY_EVIDENCE || state,
      provider,
    );
    await mkdir(evidence, { recursive: true });
    const server = spawnFixture(process.execPath, [rustUiFixture, state], {
      env: { ...process.env, FIXTURE_WORKER_COUNT: "0" },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let log = "";
    server.stderr.on("data", (chunk) => (log += chunk));
    try {
      const port = await waitForFixturePort(server, { diagnostics: () => log });
      const origin = `http://127.0.0.1:${port}`;
      const lead = (await readTestState(origin)).runtime.agents.find(
        (agent) => agent.name === "Release lead",
      );
      await page.route("**/api/accounts", async (route) => {
        const response = await route.fetch();
        const data = await response.json();
        const base = data.accounts[0];
        const providers =
          provider === "codex"
            ? ["codex", "codex", "codex", "claude", "claude"]
            : ["claude", "codex", "codex", "codex", "claude"];
        data.accounts = providers.map((accountProvider, index) => ({
          ...base,
          id: index ? `eligibility-${index}` : base.id,
          accountId: index ? `eligibility-${index}` : base.accountId,
          provider: accountProvider,
          status: "ready",
          label:
            index === 0
              ? "Current account with a very long organization name"
              : index === 1
                ? "ifdotpy+alexcodex@gmail.example.com"
                : index === 2
                  ? "A very long account name for the enterprise workspace"
                  : `Claude code ${index}`,
          email:
            index === 1
              ? "ifdotpy+alexcodex@gmail.example.com"
              : "magritte+long.organization.name@tutamail.example.com",
        }));
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
                primary: {
                  usedPercent: 12,
                  windowDurationMins: 300,
                  resetsAt: Date.now() / 1000 + 3600,
                },
              },
            },
          },
        }),
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
            Object.assign(entity.value, { provider, yoloMode: false });
            document.payload = JSON.stringify(entity);
          }
        }
        await route.fulfill({ response, json: data });
      });
      await page.setViewportSize({ width: 1440, height: 900 });
      await page.goto(origin);
      await page.locator(`[data-chat="${lead.id}"]`).click();
      await page
        .getByRole("button", { name: "Chat settings", exact: true })
        .click();
      const dialog = page.getByRole("dialog", {
        name: "Chat settings",
        exact: true,
      });
      await dialog.getByRole("tab", { name: /^Team/ }).click();
      const accounts = dialog.getByRole("region", {
        name: "Account eligibility",
        exact: true,
      });
      await expect(
        accounts.getByRole("heading", { name: "Codex 3", exact: true }),
      ).toBeVisible();
      await expect(
        accounts.getByRole("heading", { name: "Claude 2", exact: true }),
      ).toBeVisible();
      await expect(accounts.getByRole("checkbox")).toHaveCount(5);
      await expect(
        accounts.getByRole("checkbox").first(),
      ).toHaveAccessibleDescription(/@/);
      await expect(accounts.getByText("Current", { exact: true })).toHaveCount(
        1,
      );
      await expect(accounts.getByText("88% left", { exact: true })).toHaveCount(
        5,
      );
      const measurements = [];
      for (const width of [1440, 390, 320])
        for (const scheme of ["light", "dark"]) {
          await page.setViewportSize({
            width,
            height: width === 1440 ? 900 : 844,
          });
          await page.emulateMedia({ colorScheme: scheme });
          await expect(page.locator("html")).toHaveAttribute(
            "data-mantine-color-scheme",
            scheme,
          );
          const geometry = await dialog.evaluate((node) => ({
            width: node.clientWidth,
            scrollWidth: node.scrollWidth,
            rows: [...node.querySelectorAll(".eligibility-account")].map(
              (row) => ({
                height: row.getBoundingClientRect().height,
                width: row.clientWidth,
                scrollWidth: row.scrollWidth,
              }),
            ),
            text: [
              ...node.querySelectorAll(
                ".eligibility-name,.eligibility-email,.eligibility-remaining,.eligibility-current,.eligibility-provider",
              ),
            ].map((el) => ({
              text: el.textContent,
              width: el.clientWidth,
              scrollWidth: el.scrollWidth,
              overflow: getComputedStyle(el).overflowX,
              ellipsis: getComputedStyle(el).textOverflow,
            })),
            permissions: [
              ...node.querySelectorAll(
                ".execution-setting-control .mantine-SegmentedControl-label",
              ),
            ].map((el) => ({
              text: el.textContent,
              width: el.clientWidth,
              scrollWidth: el.scrollWidth,
            })),
            columns: [
              ...node.querySelectorAll(".eligibility-provider-accounts"),
            ].map(
              (el) =>
                getComputedStyle(el).gridTemplateColumns.split(" ").length,
            ),
          }));
          expect(geometry.scrollWidth).toBeLessThanOrEqual(geometry.width);
          for (const row of geometry.rows) {
            expect(row.scrollWidth).toBeLessThanOrEqual(row.width);
            expect(row.height).toBeLessThanOrEqual(width === 1440 ? 56 : 45);
          }
          for (const text of geometry.text) {
            // Ellipsized values deliberately retain their full text for titles and
            // descriptions; only their clipped viewport is painted in the row.
            if (text.scrollWidth > text.width) {
              expect(text.overflow).toBe("hidden");
              expect(text.ellipsis).toBe("ellipsis");
            }
          }
          for (const text of geometry.permissions)
            expect(text.scrollWidth, text.text).toBeLessThanOrEqual(text.width);
          expect(geometry.columns).toEqual([
            width === 1440 ? 2 : 1,
            width === 1440 ? 2 : 1,
          ]);
          measurements.push({ viewport: width, scheme, ...geometry });
          await page.screenshot({
            path: join(evidence, `Team-${width}-${scheme}.png`),
          });
        }
      await writeFile(
        join(evidence, "measurements.json"),
        JSON.stringify(measurements, null, 2),
      );
      for (const width of [1440, 390]) {
        await page.setViewportSize({ width, height: 900 });
        for (const tab of ["Models", "Team"]) {
          await dialog
            .getByRole("tab", { name: new RegExp("^" + tab) })
            .click();
          await dialog
            .getByRole("button", { name: "Close", exact: true })
            .focus();
          for (const key of ["Tab", "Shift+Tab"]) {
            for (let step = 0; step < 40; step++) {
              await page.keyboard.press(key);
              expect(
                await dialog.evaluate((node) =>
                  node.contains(document.activeElement),
                ),
                `${provider} ${tab} ${width}px ${key} step ${step}`,
              ).toBe(true);
            }
          }
          await expect(dialog.locator("details")).toHaveCount(0);
          if (provider === "claude" && tab === "Team") {
            const advanced = dialog.getByRole("button", {
              name: "More settings and actions",
              exact: true,
            });
            const compactLimit = dialog.getByLabel("Auto-compact token limit", {
              exact: true,
            });
            await expect(compactLimit).toHaveCount(0);
            await advanced.click();
            await expect(compactLimit).toBeVisible();
            const commands = dialog.getByRole("button", {
              name: "Commands and skills",
              exact: true,
            });
            const load = dialog.getByRole("button", {
              name: "Load commands",
              exact: true,
            });
            await expect(load).toHaveCount(0);
            await commands.click();
            await expect(load).toBeVisible();
            await commands.click();
            await expect(load).toHaveCount(0);
            await dialog
              .getByRole("button", { name: "Close", exact: true })
              .focus();
            for (const key of ["Tab", "Shift+Tab"])
              for (let step = 0; step < 40; step++) {
                await page.keyboard.press(key);
                expect(
                  await dialog.evaluate((node) =>
                    node.contains(document.activeElement),
                  ),
                  `expanded ${width}px ${key} step ${step}`,
                ).toBe(true);
              }
            await advanced.click();
            await expect(compactLimit).toHaveCount(0);
          }
        }
      }
      console.log(`Eligibility evidence: ${evidence}`);
    } finally {
      server.stdin?.end();
    }
  });
