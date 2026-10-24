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

test("390px composer keeps role symbols separate and accessible model names", async ({
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
    for (const { provider, model, name } of [
      {
        provider: "codex",
        model: "gpt-6-astra",
        name: "Astra 6",
      },
      {
        provider: "claude",
        model: "claude-opus-5-5",
        name: "Opus 5.5",
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
          await expect(label).toBeHidden();
          const geometry = await trigger
            .locator(".execution-symbols")
            .evaluateAll((nodes) =>
              nodes.map((node) => node.getBoundingClientRect().toJSON()),
            );
          expect(geometry).toHaveLength(3);
          const triggerBox = await trigger.boundingBox();
          for (const [index, box] of geometry.entries()) {
            expect(box.left).toBeGreaterThanOrEqual(triggerBox.x);
            expect(box.right).toBeLessThanOrEqual(
              triggerBox.x + triggerBox.width,
            );
            if (index)
              expect(box.left).toBeGreaterThanOrEqual(
                geometry[index - 1].right,
              );
            expect(box.top).toBe(geometry[0].top);
          }
          await expect(
            page.getByRole("button", { name: "Attach files", exact: true }),
          ).toBeVisible();
          measurements.push({ provider, scheme, geometry });
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
