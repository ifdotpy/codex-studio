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

test("composer keeps one tools row and readable mixed-provider execution summary in medium containers", async ({
  browser,
}) => {
  test.setTimeout(180_000);
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
      { width: 1440, target: 1100 },
      { width: 1920, target: 1400 },
      { width: 1440, target: 900 },
      { width: 1440, target: 470, team: true },
      { width: 1440, target: 600, team: true },
      { width: 1440, target: 440, team: true },
      { width: 1024, target: 480 },
      { width: 1080, target: 760 },
      { width: 768, target: 432 },
      { width: 390, target: 358 },
      { width: 320, target: 288 },
    ])
      for (const scheme of ["light", "dark"]) {
        const page = await browser.newPage({
          viewport: { width: 1440, height: 900 },
          colorScheme: scheme,
        });
        try {
          page.setDefaultTimeout(10000);
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
            .getByLabel(
              page.viewportSize().width <= 760
                ? "Back to chats"
                : "Toggle conversations",
              { exact: true },
            )
            .waitFor();
          if (!(await page.locator("#sidebar").isVisible()))
            await page
              .getByLabel(
                page.viewportSize().width <= 760
                  ? "Back to chats"
                  : "Toggle conversations",
                { exact: true },
              )
              .click();
          await page
            .locator("[data-chat]")
            .filter({ hasText: "Release lead" })
            .first()
            .click();
          await page.setViewportSize({ width, height: 900 });
          if (width < 760) await expect(page.locator("#sidebar")).toBeHidden();
          if (team) {
            await page.locator("#team-toggle").click();
            await expect(page.locator("#team")).toBeVisible();
          }
          const trigger = page.locator("#composer .execution-summary-trigger");
          await expect(trigger).toHaveCount(1);
          await expect(trigger).toHaveAttribute(
            "aria-label",
            /Lead Opus 4.6 Medium.*Workers Sol 5.6 Medium.*Review same as caller/,
          );
          await expect(
            trigger.locator(".execution-summary-segment"),
          ).toHaveCount(3);
          await expect(trigger.locator(".execution-worker-count")).toHaveCount(
            0,
          );
          await expect(trigger).not.toHaveAttribute(
            "aria-label",
            /\d+ workers/,
          );
          await expect(trigger).not.toHaveAttribute("title", /\d+ workers/);
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
          if (target >= 900) {
            const names = await trigger
              .locator(".execution-model,.execution-inherited-label")
              .evaluateAll((nodes) =>
                nodes.map((node) => ({
                  text: node.textContent,
                  actual: node.clientWidth,
                  content: node.scrollWidth,
                })),
              );
            for (const name of names)
              expect(
                name.actual,
                `${width}px ${name.text} is fully visible`,
              ).toBe(name.content);
          }
          await expect(page.locator("#toast")).toBeHidden({ timeout: 10_000 });
          await trigger.click();
          const settings = page.locator(".execution-dropdown");
          await expect(settings).toBeVisible();
          await expect(
            page.getByRole("dialog", { name: "Chat settings", exact: true }),
          ).toHaveCount(0);
          if (width > 768) {
            await expect(settings).toHaveClass(/mantine-Popover-dropdown/);
            const anchor = await trigger.boundingBox();
            const popup = await settings.boundingBox();
            expect(Math.abs(popup.x - anchor.x)).toBeLessThanOrEqual(1);
          }
          await expect(
            settings
              .getByRole("radiogroup", { name: "Agent role", exact: true })
              .getByRole("radio")
              .first(),
          ).toBeChecked();
          await page.keyboard.press("Escape");
          await expect(settings).toBeHidden();
          await expect(trigger).toBeFocused();
          const geometry = await page
            .locator("#composer")
            .evaluate((composer) => {
              const r = (element) => element.getBoundingClientRect().toJSON();
              const chips = [
                ...composer.querySelectorAll(".execution-menu"),
              ].map((button) => {
                const label = button.querySelector(".execution-model"),
                  glyph = button.querySelector(".execution-provider>svg");
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
                  namesHidden: getComputedStyle(label).display === "none",
                  names: [
                    ...button.querySelectorAll(
                      ".execution-model,.execution-inherited-label",
                    ),
                  ].map((node) => ({
                    text: node.textContent,
                    scrollWidth: node.scrollWidth,
                    clientWidth: node.clientWidth,
                    visible: getComputedStyle(node).display !== "none",
                    ...r(node),
                  })),
                  roleIcons: [
                    ...button.querySelectorAll(".execution-role-icon"),
                  ].map(r),
                  inherited: r(
                    button.querySelector(".execution-inherited-icon"),
                  ),
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
            path: join(
              evidence,
              `composer-${width}-${target}-${team ? "docked" : "closed"}-${scheme}.png`,
            ),
          });
          if (target === 760 && scheme === "light") {
            await page
              .locator('.composer-tools input[type="file"]')
              .setInputFiles({
                name: "notes.txt",
                mimeType: "text/plain",
                buffer: Buffer.from("Release notes"),
              });
            const previews = page.locator(".composer-tools .attachment-list");
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
                    .querySelector(".composer-tools .attachment-list")
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
        // Phone composers (25em) keeps role symbols only.
        if (result.composer.width > 400) expect(chip.namesHidden).toBe(false);
        const names = chip.names.filter((name) => name.visible);
        // One-line role symbols leave room for the third name from 32em.
        if (result.composer.width >= 500) expect(names).toHaveLength(3);
        for (const name of names) {
          expect(name.left).toBeGreaterThanOrEqual(chip.button.left);
          expect(name.right).toBeLessThanOrEqual(chip.button.right);
          if (result.composer.width >= 900)
            expect(name.scrollWidth, JSON.stringify(result)).toBe(
              name.clientWidth,
            );
        }
        if (!chip.namesHidden)
          expect(
            chip.label.width,
            JSON.stringify(result),
          ).toBeGreaterThanOrEqual(chip.minimum - 1);

        expect(chip.roleIcons).toHaveLength(3);
        for (const icon of [...chip.roleIcons, chip.inherited])
          expect(icon.width).toBeGreaterThanOrEqual(14);
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
