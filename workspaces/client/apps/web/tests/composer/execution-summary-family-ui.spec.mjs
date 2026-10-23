import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
} from "../playwright.mjs";
import { waitForFixturePort } from "../fixture-readiness.mjs";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("390px fine-pointer composer shows a complete Codex or Claude family name", async ({
  browser,
}) => {
  test.setTimeout(90_000);
  const state = await mkdtemp(join(tmpdir(), "execution-family-"));
  const evidence = process.env.EXECUTION_FAMILY_EVIDENCE || state;
  await mkdir(evidence, { recursive: true });
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { FIXTURE_HOLD: "lead" },
  });
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += chunk));
  const measurements = [];
  try {
    const port = await waitForFixturePort(fixture, { diagnostics: () => log });
    const origin = `http://127.0.0.1:${port}`;
    const base = await readTestState(origin),
      contract = await readFixtureSyncContract(origin);
    for (const { provider, model, name, family } of [
      {
        provider: "codex",
        model: "gpt-6-astra",
        name: "Astra 6",
        family: "Astra",
      },
      {
        provider: "claude",
        model: "claude-opus-5-5",
        name: "Opus 5.5",
        family: "Opus",
      },
    ])
      for (const scheme of ["light", "dark"]) {
        const snapshot = structuredClone(base);
        const lead = snapshot.threads.find(
          (agent) => agent.name === "Release lead",
        );
        Object.assign(lead, {
          provider,
          model,
          effort: "medium",
          workerDefaults: {
            model: "gpt-6-luna",
            effort: "high",
            accountKey: "default",
          },
        });
        snapshot.runtime.agents = snapshot.threads.filter(
          (agent) => agent.source === "managed",
        );
        const page = await browser.newPage({
          // Touch phones show symbols only; their 44px targets leave no name room.
          viewport: { width: 390, height: 844 },
          colorScheme: scheme,
        });
        try {
          await stubEntityState(page, snapshot, contract);
          await page.route("**/api/sync/pull**", (route) =>
            new URL(route.request().url()).searchParams.get("scope") ===
            "state:entities:v1"
              ? route.fallback()
              : route.continue(),
          );
          await page.route(
            (url) => url.pathname === "/api/models",
            (route) =>
              route.fulfill({
                json: {
                  data: [
                    { model, displayName: name, provider },
                    {
                      model: "gpt-6-luna",
                      displayName: "Luna 6",
                      provider: "codex",
                    },
                  ].map((row) => ({
                    ...row,
                    defaultReasoningEffort: "medium",
                    supportedReasoningEfforts: [
                      { reasoningEffort: "medium" },
                      { reasoningEffort: "high" },
                    ],
                    serviceTiers: [],
                  })),
                },
              }),
          );
          await page.goto(origin);
          // A fresh phone page opens the chats board.
          await page.locator("#sidebar [data-chat]").first().waitFor();
          await page
            .locator("[data-chat]")
            .filter({ hasText: "Release lead" })
            .first()
            .click();
          await expect(page.locator("#sidebar")).toBeHidden();
          const trigger = page.locator("#composer .execution-summary-trigger");
          await expect(trigger).toHaveAttribute(
            "aria-label",
            new RegExp(`Lead ${name} Medium`),
          );
          await expect(trigger).toHaveAttribute(
            "title",
            new RegExp(`Lead ${name} Medium`),
          );
          const label = trigger.locator('[data-role="lead"] .execution-model');
          await expect(label.locator(".execution-model-family")).toBeVisible();
          expect(await label.innerText()).toBe(family);
          const geometry = await label.evaluate((node) => ({
            text: node.innerText,
            width: node.clientWidth,
            scrollWidth: node.scrollWidth,
            box: node.getBoundingClientRect().toJSON(),
          }));
          expect(
            geometry.scrollWidth,
            `${provider} family is untruncated`,
          ).toBe(geometry.width);
          expect(geometry.text).toMatch(/^[A-Za-z]+$/);
          const triggerBox = await trigger.boundingBox();
          expect(geometry.box.left).toBeGreaterThanOrEqual(triggerBox.x);
          expect(geometry.box.right).toBeLessThanOrEqual(
            triggerBox.x + triggerBox.width,
          );
          measurements.push({ provider, scheme, ...geometry });
          await expect(page.locator("#toast")).toBeHidden({ timeout: 10_000 });
          await page.mouse.move(0, 0);
          await page.screenshot({
            path: join(evidence, `family-390-${provider}-${scheme}.png`),
          });
          await page.setViewportSize({ width: 1440, height: 900 });
          await expect(label.locator(".execution-model-family")).toBeHidden();
          expect(await label.innerText()).toBe(name);
        } finally {
          await page.context().close();
        }
      }
    await writeFile(
      join(evidence, "measurements.json"),
      JSON.stringify(measurements, null, 2),
    );
    console.log(`Family-name evidence: ${evidence}`);
  } finally {
    fixture.kill("SIGTERM");
  }
});
