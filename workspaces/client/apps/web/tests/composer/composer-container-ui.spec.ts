import {
  test,
  expect,
  spawnFixture,
  rustUiFixture,
  readTestState,
  readFixtureSyncContract,
  stubEntityState,
} from "../playwright.ts";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("composer keeps one tools row and readable mixed-provider execution summary in medium containers", async ({
  browser,
}, testInfo) => {
  test.setTimeout(180_000);
  const state = await mkdtemp(join(tmpdir(), "studio-composer-container-"));
  const evidence = testInfo.outputPath("evidence");
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
      reviewDefaults: { model: "gpt-6.1-sol", effort: "medium" },
      accountTransfer: {
        id: "blocked-move",
        status: "pending",
        total: 2,
        completed: 1,
        blocked: [
          { id: lead.id, name: lead.name, reason: "Account needs sign-in" },
        ],
      },
      workerDefaults: {
        model: "gpt-6.1-sol",
        effort: "medium",
        accountKey: "default",
      },
    });
    const { workspaceId } = await (
      await fetch(origin + "/api/sync/identity")
    ).json();
    const draftResponse = await fetch(origin + "/api/sync/drafts", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": snapshot.token,
        "X-Canvas-Workspace": workspaceId,
        Origin: origin,
      },
      body: JSON.stringify({
        rows: ["Current draft", "Saved alternative"].map((text, index) => ({
          newDocumentState: {
            id: `composer-layout-${index}:${lead.id}`,
            seq: 0,
            deleted: false,
            payload: JSON.stringify({
              id: `composer-layout-${index}:${lead.id}`,
              session: lead.id,
              device: `composer-layout-${index}`,
              text,
              updated: Date.now() + index,
            }),
          },
        })),
      }),
    });
    expect(draftResponse.ok, await draftResponse.text()).toBe(true);
    snapshot.runtime.agents = snapshot.threads.filter(
      (agent) => agent.source === "managed",
    );
    const models = [
      ["claude-opus-4-6", "Opus 4.6", "claude"],
      ["gpt-6.1-sol", "Sol 6.1", "codex"],
    ].map(([model, displayName, provider]) => ({
      model,
      displayName,
      provider,
      defaultReasoningEffort: "medium",
      supportedReasoningEfforts: [{ reasoningEffort: "medium" }],
      serviceTiers: [],
    }));
    for (const { width, target, team } of [
      { width: 1024, target: 700 },
      { width: 1440, target: 1100 },
      { width: 1920, target: 1400 },
      { width: 1440, target: 900 },
      { width: 1440, target: 470, team: true },
      { width: 1440, target: 600, team: true },
      { width: 1440, target: 440, team: true },
      { width: 1024, target: 480 },
      { width: 768, target: 432 },
      { width: 390, target: 358 },
      { width: 320, target: 288 },
      { width: 1080, target: 760 },
    ])
      for (const scheme of ["light", "dark"]) {
        const page = await browser.newPage({
          viewport: { width: 1440, height: 900 },
          colorScheme: scheme,
          hasTouch: width <= 768,
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
          await expect(page.locator("#stop")).toBeVisible();
          await expect(page.locator("#send")).toBeVisible();
          await expect(
            page.getByRole("button", { name: /^Other drafts/ }),
          ).toBeVisible();
          await expect(trigger).toHaveAttribute(
            "aria-label",
            /Lead Opus 4.6 Medium.*Workers Sol 6.1 Medium.*Review Sol 6.1 Medium/,
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
          const roleWords = trigger.locator(".execution-role:visible");
          const secondaryModels = trigger.locator(
            '.execution-summary-segment:not([data-role="lead"]) .execution-model:visible',
          );
          if (target >= 760) {
            await expect(roleWords).toHaveCount(3);
            await expect(secondaryModels).toHaveCount(2);
          } else if (target >= 600) {
            await expect(roleWords).toHaveCount(0);
            await expect(secondaryModels).toHaveCount(0);
            await expect(
              trigger.locator(".execution-symbols:visible"),
            ).toHaveCount(3);
          } else if (target <= 440) {
            await expect(trigger.locator(".execution-summary-more")).toHaveText(
              "+2",
            );
            await expect(
              trigger.locator(".execution-summary-more"),
            ).toBeVisible();
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
              const trigger = composer.querySelector(
                ".execution-summary-trigger",
              );
              const visible = (node) =>
                node.getClientRects().length &&
                getComputedStyle(node).display !== "none";
              const chips = [
                {
                  button: r(trigger),
                  names: [
                    ...trigger.querySelectorAll(
                      ".execution-model,.execution-inherited-label",
                    ),
                  ]
                    .filter(visible)
                    .map((node) => ({
                      text: node.textContent,
                      scrollWidth: node.scrollWidth,
                      clientWidth: node.clientWidth,
                      ...r(node),
                    })),
                  segments: [
                    ...trigger.querySelectorAll(".execution-summary-segment"),
                  ]
                    .filter(visible)
                    .map(r),
                  parts: [
                    ...trigger.querySelectorAll(
                      ".execution-symbols,.execution-summary-more",
                    ),
                  ]
                    .filter(visible)
                    .map(r),
                  roleIcons: trigger.querySelectorAll(".execution-role-icon")
                    .length,
                },
              ];
              return {
                composer: r(composer),
                chips,
                status: r(composer.querySelector(".account-transfer-chip")),
                bar: r(composer.querySelector(".composer-bar")),
                controls: [
                  ...composer.querySelectorAll(
                    ".composer-draft-control button,.composer-tools button,.composer-submit-actions>button",
                  ),
                ]
                  .filter(
                    (button) =>
                      button.getClientRects().length &&
                      !button.closest("details:not([open])"),
                  )
                  .map((button) => ({
                    name: button.getAttribute("aria-label"),
                    inRow:
                      getComputedStyle(button).position !== "absolute" &&
                      getComputedStyle(button.parentElement).position !==
                        "absolute",
                    ...r(button),
                  })),
              };
            });
          results.push({ width, target, team: !!team, scheme, ...geometry });
          await writeFile(
            join(evidence, "geometry.json"),
            JSON.stringify(results, null, 2),
          );
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
                    ".composer-draft-control button,.composer-tools button,.composer-submit-actions>button",
                  ),
                ].filter(
                  (button) =>
                    button.getClientRects().length &&
                    !button.closest(".attachment-list,details:not([open])") &&
                    getComputedStyle(button).position !== "absolute" &&
                    getComputedStyle(button.parentElement).position !==
                      "absolute",
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
      const centers = result.controls
        .filter((control) => control.inRow)
        .map((control) => control.top + control.height / 2);
      expect(
        Math.max(...centers) - Math.min(...centers),
        JSON.stringify(result),
      ).toBeLessThanOrEqual(1);
      for (const control of result.controls) {
        expect(control.left, JSON.stringify(result)).toBeGreaterThanOrEqual(
          result.composer.left,
        );
        expect(control.right, JSON.stringify(result)).toBeLessThanOrEqual(
          result.composer.right,
        );
      }
      for (let i = 0; i < result.controls.length; i++)
        for (let j = i + 1; j < result.controls.length; j++) {
          const a = result.controls[i],
            b = result.controls[j];
          expect(
            Math.min(a.right, b.right) > Math.max(a.left, b.left) &&
              Math.min(a.bottom, b.bottom) > Math.max(a.top, b.top),
            JSON.stringify({ a, b }),
          ).toBe(false);
        }
      for (const chip of result.chips) {
        for (const part of chip.parts) {
          expect(part.left, JSON.stringify(result)).toBeGreaterThanOrEqual(
            chip.button.left,
          );
          expect(part.right, JSON.stringify(result)).toBeLessThanOrEqual(
            chip.button.right,
          );
          expect(part.width).toBeGreaterThan(0);
        }
        for (const name of chip.names) {
          expect(name.left, JSON.stringify(result)).toBeGreaterThanOrEqual(
            chip.button.left,
          );
          expect(name.right, JSON.stringify(result)).toBeLessThanOrEqual(
            chip.button.right,
          );
          expect(name.scrollWidth, JSON.stringify(result)).toBe(
            name.clientWidth,
          );
        }
        for (let i = 1; i < chip.segments.length; i++)
          expect(
            chip.segments[i].left,
            JSON.stringify(result),
          ).toBeGreaterThanOrEqual(chip.segments[i - 1].right);
        expect(
          chip.names.length,
          "Lead name stays visible",
        ).toBeGreaterThanOrEqual(1);
        expect(chip.roleIcons).toBe(0);
      }
      expect(
        result.status.bottom,
        "Account status has its own row",
      ).toBeLessThanOrEqual(result.bar.top);
    }
    console.log("Composer evidence:", evidence);
  } finally {
    fixture.kill("SIGTERM");
  }
});
