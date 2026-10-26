import type { components } from "../../src/generated/api.ts";
import { required } from "../../src/components/componentTestFixtures.ts";
import { createFixtureViteServer as createServer } from "../playwright.ts";
import type { Snapshot } from "../../src/types.ts";
import { fixturePostBody } from "../misc-fixture.ts";
import { fixtureOrigin } from "../misc-fixture.ts";
import type {} from "./radio-create-browser.fixture.tsx";
import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { modelValue } from "../fixtures/model-picker.ts";
import { chooseSetupValue } from "../fixtures/setup-controls.ts";
import { handleEntitySyncFixtureRequest, test, expect } from "../playwright.ts";

test("radio create browser", async ({ page: runnerPage }) => {
  test.setTimeout(90000);
  const root = resolve(import.meta.dirname, "../../");

  const cacheDir = await mkdtemp(join(tmpdir(), "studio-radio-create-"));
  const server = await createServer({
    configFile: false,
    root,
    cacheDir,
    server: {
      host: "127.0.0.1",
      port: 0,
      watch: { ignored: ["**/test-results/**", "**/playwright-report/**"] },
    },
    plugins: [
      {
        name: "fixture",
        configureServer(s) {
          s.middlewares.use((req, res, next) => {
            if (
              handleEntitySyncFixtureRequest(req, res, {
                workspaceId: "1234567890abcdef1234567890abcdef",
                snapshot: {
                  stateDir: "create-test",
                  chats: [],
                  edges: [],
                  runtime: {
                    agents: [],
                    rooms: [],
                    tasks: [],
                    monitors: [],
                    complaints: [],
                    projects: [],
                    events: [],
                    peerTeams: [],
                    requests: [],
                    rules: [],
                    work: [],
                    nativeNotices: [],
                    sidebarOrder: null,
                    rateLimits: null,
                    rateLimitsByAccount: {},
                  },
                },
                onStreamReady: (notify) =>
                  notify([
                    { kind: "models" },
                    ...[
                      "codex",
                      "claude",
                      ...Array.from({ length: 5 }, (_, i) => "extra" + i),
                    ].map((accountKey) => ({
                      kind: "limits" as const,
                      accountKey,
                    })),
                  ]),
              })
            )
              return;
            next();
          });
          s.middlewares.use("/check", (_req, res) => {
            res.setHeader("Content-Type", "text/html");
            res.end(
              '<html><body><div id="root"></div><script type="module" src="/tests/radio/radio-create-browser.fixture.tsx"></script></body></html>',
            );
          });
        },
      },
    ],
  });
  try {
    await server.listen();
    const page = runnerPage;
    await page.setViewportSize({ width: 1100, height: 950 });
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    let state: Snapshot;
    let mode = "lost";
    const requests: components["schemas"]["PeerTeamRadioCreateRequest"][] = [];
    const identities = new Map<string, string>();
    await page.route("**/api/models?*", (r) =>
      r.fulfill({
        json: {
          data: r.request().url().includes("claude")
            ? [
                {
                  model: "opus",
                  displayName: "Claude · Opus",
                  description: "Opus 5.5 · Native model",
                  isDefault: true,
                  supportedReasoningEfforts: [{ reasoningEffort: "high" }],
                },
              ]
            : [
                { model: "gpt-6-astra", displayName: "Astra", isDefault: true },
                { model: "gpt-6-luna", displayName: "Luna" },
              ],
        },
      }),
    );
    await page.route("**/api/limits?*", (r) =>
      r.fulfill({
        json: {
          accountKey: new URL(r.request().url()).searchParams.get(
            "account_key",
          ),
          at: Date.now() / 1000,
          data: {
            rateLimits: {
              primary: {
                usedPercent: 30,
                windowDurationMins: 300,
                resetsAt: Date.now() / 1000 + 3600,
              },
            },
          },
        },
      }),
    );
    await page.route("**/snapshot", (r) => r.fulfill({ json: state }));
    await page.route("**/api/peer-teams", async (r) => {
      const body = fixturePostBody<"/api/peer-teams">(
        r.request().postDataJSON(),
      );
      if (!("radio_action" in body) || body.radio_action !== "create")
        throw new Error("Expected shared creation request");
      requests.push(body);
      if (!identities.has(body.request_id ?? ""))
        identities.set(
          body.request_id ?? "",
          identities.size ? "radio:second" : "radio:direct",
        );
      const roomId = identities.get(body.request_id ?? "");
      if (!roomId) throw new Error("Missing shared chat identity");
      assert.equal(body.radio_action, "create");
      assert.equal(body.participants?.length, 2);
      state = {
        ...state,
        threads: (body.participants ?? []).map((p, i) => ({
          id: "agent" + i,
          name: "Internal " + i,
          cwd: "/p",
          source: "managed",
          isLead: true,
          sharedRoomId: roomId,
          ...p,
        })),
        runtime: {
          ...state.runtime,
          rooms: [
            {
              id: roomId,
              name: body.name || "Shared chat",
              projectPath: "/p",
              kind: "private",
              members: ["agent0", "agent1"],
              radio: {
                direct: true,
                teamId: "direct",
                revision: 0,
                status: "idle",
                speaker: null,
                next: [],
                active: null,
                error: null,
              },
            },
          ],
        },
      };
      if (mode === "lost") {
        mode = "ok";
        return r.abort("failed");
      }
      return r.fulfill({ json: { room: state.runtime.rooms[0] } });
    });
    await page.goto(`${fixtureOrigin(server)}/check`);
    await page.waitForFunction(() => window.radioCreateFixture.fixture);
    state = await page.evaluate(() => window.radioCreateFixture.fixture);
    const form = page.getByRole("form", { name: "Create shared chat" });
    const create = form.getByRole("button", {
      name: "Create",
      exact: true,
    });
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLButtonElement>("form button[type=submit]")
          ?.disabled === false,
    );
    await form.getByRole("button", { name: "Settings for agent 1" }).click();
    assert.equal(
      await modelValue(page.getByLabel("Model for agent 1", { exact: true })),
      "gpt-6-astra",
    );
    const roles = page.getByRole("radiogroup", {
      name: "Agent role",
      exact: true,
    });
    for (const name of ["Lead", "Workers", "Review"])
      await expect(roles.getByRole("radio", { name, exact: true })).toHaveCount(
        1,
      );
    for (const name of ["Workers", "Review"]) {
      await roles.getByText(name, { exact: true }).click();
      await expect(
        page.getByText(
          "Shared chat participants manage their own worker and review defaults. These settings are inherited and cannot be changed here.",
        ),
      ).toBeVisible();
      await expect(
        page.getByRole("option", { name: "Inherited", exact: true }),
      ).toBeDisabled();
    }
    await roles.getByText("Lead", { exact: true }).click();
    assert.equal(
      await modelValue(page.getByLabel("Model for agent 1", { exact: true })),
      "gpt-6-astra",
    );

    await chooseSetupValue(
      page.getByRole("radiogroup", {
        name: "Account for agent 1 provider",
        exact: true,
      }),
      "claude",
    );
    await expect(
      page.getByLabel("Account for agent 1", { exact: true }),
    ).toHaveAttribute("data-value", "codex");
    await expect(page.locator(".setup-account")).toHaveCount(3);
    await chooseSetupValue(
      page.getByRole("radiogroup", {
        name: "Account for agent 1 provider",
        exact: true,
      }),
      "codex",
    );
    await form.getByRole("button", { name: "Settings for agent 2" }).click();
    await expect(
      form.getByRole("button", { name: "Settings for agent 1" }),
    ).toHaveAttribute("aria-expanded", "false");
    await expect(
      page.getByRole("dialog", { name: "Settings for agent 1" }),
    ).toBeHidden();
    await expect(
      form.getByRole("button", { name: "Settings for agent 2" }),
    ).toHaveAttribute("aria-expanded", "true");
    assert.equal(
      await modelValue(page.getByLabel("Model for agent 2", { exact: true })),
      "opus",
    );
    assert.match(
      await page.getByLabel("Model for agent 2", { exact: true }).innerText(),
      /Opus 5.5/,
    );
    await page.keyboard.press("Escape");
    await expect(
      form.getByRole("button", { name: "Settings for agent 2" }),
    ).toHaveAttribute("aria-expanded", "false");
    await expect(
      page.getByRole("dialog", { name: "Settings for agent 2" }),
    ).toBeHidden();
    await form.getByLabel("Chat name").fill("Architecture discussion");
    const screenshots = join(tmpdir(), "refresh-shared-screenshots");
    await mkdir(screenshots, { recursive: true });
    for (const [profile, width, colorScheme] of [
      ["dark", 1100, "dark"],
      ["light", 1100, "light"],
      ["mobile", 390, "dark"],
    ] as const) {
      await page.setViewportSize({ width, height: 950 });
      await page.emulateMedia({ colorScheme });
      await page.evaluate((scheme) => {
        document.documentElement.setAttribute(
          "data-mantine-color-scheme",
          scheme,
        );
      }, colorScheme);
      assert.equal(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
        true,
      );
      await page.screenshot({
        path: join(screenshots, `${profile}-shared.png`),
      });
      await form.getByRole("button", { name: "Settings for agent 1" }).click();
      await expect(
        form.getByRole("button", { name: "Settings for agent 2" }),
      ).toHaveAttribute("aria-expanded", "false");
      await expect(
        page.getByRole("dialog", { name: "Settings for agent 2" }),
      ).toBeHidden();
      await expect(
        page.getByLabel("Account for agent 1", { exact: true }),
      ).toBeVisible();
      await expect(page.locator(".shared-create-picker")).toBeInViewport({
        ratio: 1,
      });
      await expect(page.locator(".setup-account")).toHaveCount(4);
      await page.screenshot({
        path: join(screenshots, `${profile}-shared-picker.png`),
      });
      await page.keyboard.press("Escape");
    }
    await page.setViewportSize({ width: 1100, height: 950 });
    await create.click();
    await page.getByRole("button", { name: "Retry creation" }).waitFor();
    assert.equal(
      await page.evaluate(() => window.radioCreateFixture.opened),
      null,
    );
    assert.equal(await form.getByLabel("Chat name").isDisabled(), true);
    await page.reload();
    await page.getByRole("button", { name: "Retry creation" }).click();
    await page.waitForFunction(
      () => window.radioCreateFixture.opened === "radio:direct",
    );
    assert.equal(requests.length, 2);
    assert.deepEqual(requests[0], requests[1]);
    const { request_id, ...body } = required(requests[0]);
    assert.match(request_id, /^[0-9a-f-]{36}$/);
    assert.deepEqual(body, {
      action: "radio",
      radio_action: "create",
      path: "/p",
      name: "Architecture discussion",
      participants: [
        { account_key: "codex", model: "gpt-6-astra" },
        { account_key: "claude", model: "opus" },
      ],
    });
    assert.equal(await page.locator(".sidebar-row").count(), 1);
    assert.equal(await page.locator(".peer-team").count(), 0);
    assert.equal(
      await page.getByText("Internal 0", { exact: true }).count(),
      0,
    );
    assert.equal(
      await page.locator(".sidebar-row strong").innerText(),
      "Architecture discussion",
    );
    await page.getByRole("button", { name: "New chat", exact: true }).click();
    assert.equal(
      await page.evaluate(() => window.radioCreateFixture.ordinary),
      true,
    );
    await page
      .getByRole("button", { name: "New shared chat", exact: true })
      .click();
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLButtonElement>("form button[type=submit]")
          ?.disabled === false,
    );
    await page.evaluate(() => (window.radioCreateFixture.failRefresh = true));
    await create.click();
    await page.getByRole("button", { name: "Refresh", exact: true }).waitFor();
    const posted = requests.length;
    await page.evaluate(() => (window.radioCreateFixture.failRefresh = false));
    await page.getByRole("button", { name: "Refresh", exact: true }).click();
    await form.waitFor({ state: "detached" });
    assert.equal(requests.length, posted);
    const projectOptions = page.getByRole("button", {
      name: "Options for project Project",
    });
    await projectOptions.locator("..").hover();
    await projectOptions.click();
    await page.getByRole("menuitem", { name: "New shared chat" }).click();
    await expect(create).toBeEnabled();
    await form.getByRole("button", { name: "Settings for agent 1" }).click();
    await page.locator('[data-account-key="extra0"]').click();
    await expect(
      page.getByLabel("Model for agent 1", { exact: true }),
    ).toHaveAttribute("data-value", "gpt-6-astra");
    await page.getByRole("option", { name: "Luna 6", exact: true }).click();
    await page.keyboard.press("Escape");
    await form.getByRole("button", { name: "Settings for agent 2" }).click();
    await chooseSetupValue(
      page.getByLabel("Reasoning for agent 2", { exact: true }),
      "high",
    );
    await page.keyboard.press("Escape");
    await create.click();
    await form.waitFor({ state: "detached" });
    assert.equal(required(requests.at(-1)).path, "/p");
    assert.deepEqual(required(requests.at(-1)).participants, [
      { account_key: "extra0", model: "gpt-6-luna" },
      { account_key: "claude", model: "opus", effort: "high" },
    ]);
    expect(errors).toEqual([]);
    console.log(
      `PASS direct shared creation, two account/model selections, exact retry after reload, acknowledged refresh, one sidebar row, ordinary chat and project entry. Screenshots ${screenshots}`,
    );
  } finally {
    await server.close();
    await rm(cacheDir, { recursive: true, force: true });
  }
});
