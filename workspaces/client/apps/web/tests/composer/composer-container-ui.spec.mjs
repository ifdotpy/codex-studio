import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
} from "../playwright.mjs";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("composer keeps one tools row and readable mixed-provider chips in medium containers", async ({
  browser,
}) => {
  test.setTimeout(120_000);
  const state = await mkdtemp(join(tmpdir(), "studio-composer-container-"));
  const evidence =
    process.env.COMPOSER_CONTAINER_EVIDENCE || join(state, "evidence");
  await mkdir(evidence, { recursive: true });
  const fixture = spawnFixture(process.execPath, [rustUiFixture, state], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { FIXTURE_HOLD: "lead" },
  });
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += chunk));
  const results = [];
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await readTestState(origin),
      contract = await readFixtureSyncContract(origin);
    const lead = snapshot.threads.find(
      (agent) => agent.name === "Release lead",
    );
    Object.assign(lead, {
      provider: "claude",
      model: "claude-opus-4-6",
      effort: "medium",
      workerDefaults: {
        model: "gpt-5.6-sol",
        effort: "medium",
        accountKey: "default",
      },
    });
    snapshot.runtime.agents = snapshot.threads.filter(
      (agent) => agent.source === "managed",
    );
    const models = [
      ["claude-opus-4-6", "Opus 4.6", "claude"],
      ["gpt-5.6-sol", "Sol 5.6", "codex"],
    ].map(([model, displayName, provider]) => ({
      model,
      displayName,
      provider,
      defaultReasoningEffort: "medium",
      supportedReasoningEfforts: [{ reasoningEffort: "medium" }],
      serviceTiers: [],
    }));
    for (const { width, target, team } of [
      { width: 1440, target: 600 },
      { width: 1024, target: 480 },
      { width: 1080, target: 760 },
      { width: 768, target: 432 },
    ])
      for (const scheme of ["light", "dark"]) {
        const page = await browser.newPage({
          viewport: { width, height: 900 },
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
            (route) => route.fulfill({ json: { data: models } }),
          );
          await page.goto(origin);
          await page
            .locator("[data-chat]")
            .filter({ hasText: "Release lead" })
            .first()
            .click();
          if (team) {
            await page.locator("#team-toggle").click();
            await expect(page.locator("#team")).toBeVisible();
          }
          await expect(
            page.locator("#composer .execution-model").first(),
          ).toContainText("Opus");
          await page.evaluate(
            ({ target, scheme }) => {
              document.documentElement.setAttribute(
                "data-mantine-color-scheme",
                scheme,
              );
              const workspace = document.querySelector(".workspace");
              const gutter = parseFloat(
                getComputedStyle(workspace).getPropertyValue("--chat-gutter"),
              );
              // The same column ratio owned by the Studio content-width preference.
              document.documentElement.style.setProperty(
                "--studio-content-width-ratio",
                String(target / (workspace.clientWidth - 2 * gutter)),
              );
            },
            { target, scheme },
          );
          const geometry = await page
            .locator("#composer")
            .evaluate((composer) => {
              const r = (element) => element.getBoundingClientRect().toJSON();
              const chips = [
                ...composer.querySelectorAll(".execution-menu"),
              ].map((button) => {
                const label = button.querySelector(".execution-model"),
                  glyph = button.querySelector(".execution-selected>svg");
                const probe = document.createElement("span");
                probe.style.cssText =
                  "display:block;width:var(--studio-composer-model-min);position:absolute";
                label.append(probe);
                const minimum = probe.getBoundingClientRect().width;
                probe.remove();
                return {
                  button: r(button),
                  label: r(label),
                  glyph: r(glyph),
                  minimum,
                  glyphColor: getComputedStyle(glyph).color,
                };
              });
              return {
                composer: r(composer),
                chips,
                controls: [
                  ...composer.querySelectorAll(
                    ".composer-tools button,.composer-submit-actions>button",
                  ),
                ]
                  .filter(
                    (button) =>
                      button.getClientRects().length &&
                      !button.closest("details:not([open])"),
                  )
                  .map((button) => ({
                    name: button.getAttribute("aria-label"),
                    ...r(button),
                  })),
              };
            });
          results.push({ width, target, team: !!team, scheme, ...geometry });
          await expect(page.locator("#toast")).toBeHidden({ timeout: 10_000 });
          await page.screenshot({
            path: join(evidence, `composer-${target}-${scheme}.png`),
          });
          if (target === 760 && scheme === "light") {
            await page
              .locator('.composer-tools-inline input[type="file"]')
              .setInputFiles({
                name: "notes.txt",
                mimeType: "text/plain",
                buffer: Buffer.from("Release notes"),
              });
            const previews = page.locator(
              ".composer-tools-inline .attachment-list",
            );
            await expect(previews).toBeVisible();
            const rows = await page
              .locator("#composer")
              .evaluate((composer) => {
                const controls = [
                  ...composer.querySelectorAll(
                    ".composer-tools button,.composer-submit-actions>button",
                  ),
                ].filter(
                  (button) =>
                    button.getClientRects().length &&
                    !button.closest(".attachment-list,details:not([open])"),
                );
                return {
                  centers: controls.map((button) => {
                    const r = button.getBoundingClientRect();
                    return r.top + r.height / 2;
                  }),
                  previewBottom: composer
                    .querySelector(".composer-tools-inline .attachment-list")
                    .getBoundingClientRect().bottom,
                  controlTop: Math.min(
                    ...controls.map(
                      (button) => button.getBoundingClientRect().top,
                    ),
                  ),
                };
              });
            expect(
              Math.max(...rows.centers) - Math.min(...rows.centers),
            ).toBeLessThanOrEqual(1);
            expect(rows.previewBottom).toBeLessThanOrEqual(rows.controlTop);
            await page.screenshot({
              path: join(evidence, "composer-760-light-attachment.png"),
            });
          }
        } finally {
          await page.context().close();
        }
      }
    await writeFile(
      join(evidence, "geometry.json"),
      JSON.stringify(results, null, 2),
    );
    for (const result of results) {
      const centers = result.controls.map(
        (control) => control.top + control.height / 2,
      );
      expect(
        Math.max(...centers) - Math.min(...centers),
        JSON.stringify(result),
      ).toBeLessThanOrEqual(1);
      for (const chip of result.chips) {
        expect(chip.label.width, JSON.stringify(result)).toBeGreaterThanOrEqual(
          chip.minimum - 1,
        );
        expect(chip.glyph.width, JSON.stringify(result)).toBeGreaterThanOrEqual(
          12,
        );
        expect(chip.glyph.height).toBeGreaterThanOrEqual(12);
      }
    }
    console.log("Composer evidence:", evidence);
  } finally {
    fixture.kill("SIGTERM");
  }
});
