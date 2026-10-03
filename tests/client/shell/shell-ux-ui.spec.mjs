import { test } from "../playwright.mjs";
// Production bundle, isolated backend, no model calls.
import assert from "node:assert/strict";
import { spawnFixture as spawn } from "../playwright.mjs";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
test("Shell ux ui", async ({
  browser: testBrowser,
  page: runnerPage,
  context: runnerContext,
}) => {
  test.setTimeout(300_000);
  const repo = fileURLToPath(new URL("../../../", import.meta.url));
  const evidence = await mkdtemp(join(tmpdir(), "studio-ux-navigation-"));
  const fixture = spawn(
    "python3",
    ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
    {
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => {
    log += chunk;
  });
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(new Error(log)));
    });
    const origin = `http://127.0.0.1:${port}`;
    const snapshot = await (await fetch(origin + "/api/state")).json();
    const page = runnerPage;
    await page.setViewportSize({ width: 1440, height: 960 });
    page.setDefaultTimeout(12000);
    const errors = [],
      actions = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.route("**/api/action", (route) => {
      actions.push(route.request().postDataJSON());
      return route.fulfill({
        json: {
          receipt: { requestId: route.request().postDataJSON().request_id },
          outcome: { status: "acknowledged" },
        },
      });
    });
    await page.goto(origin);
    await page.locator("#message").waitFor();
    await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
    await page
      .getByRole("button", { name: "Close conversations", exact: true })
      .click();
    await page.locator("#sidebar").waitFor({ state: "hidden" });
    await page.reload();
    await page.locator("#message").waitFor();
    assert.equal(await page.locator("#sidebar").count(), 0);
    await page.locator("#sidebar-toggle").click();
    await page.locator("#sidebar").waitFor();
    await page.locator("#team-toggle").click();
    await page.locator("#team").waitFor();
    await page.locator("#team-close").click();
    await page.locator("#team").waitFor({ state: "hidden" });
    await page
      .getByRole("button", { name: "Search chats", exact: true })
      .click();
    await page.getByRole("dialog").waitFor();
    await page.keyboard.press("Escape");
    await page.keyboard.press("Control+k");
    await page.getByRole("dialog").waitFor();
    await page.keyboard.press("Escape");
    const other = snapshot.threads.find(
      (item) => item.name === "Other project",
    );
    await page.evaluate(
      (agentId) =>
        window.dispatchEvent(
          new CustomEvent("studio-navigate", {
            detail: { agentId, section: "messages" },
          }),
        ),
      other.id,
    );
    await page.waitForFunction(
      () =>
        document.querySelector("#conversation-title").textContent ===
        "Other project",
    );
    assert.equal(
      await page.getByRole("dialog", { name: "Messages", exact: true }).count(),
      0,
      "Navigation without a message selects the chat without opening Messages",
    );
    await page.getByRole("button", { name: "Messages" }).click();
    await page.getByRole("dialog", { name: "Messages", exact: true }).waitFor();
    await page.keyboard.press("Escape");
    await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
    for (const width of [1440, 390, 320]) {
      await page.setViewportSize({ width, height: 960 });
      for (const scheme of ["light", "dark"]) {
        await page
          .getByRole("button", { name: "Studio settings", exact: true })
          .click();
        const dialog = page.getByTestId("studio-settings");
        await dialog.waitFor({ state: "visible" });
        assert.deepEqual(
          (await dialog.getByRole("tab").allTextContents()).map((label) =>
            label.trim(),
          ),
          ["Accounts", "Appearance", "Hotkeys"],
        );
        await dialog
          .getByRole("tab", { name: "Appearance", exact: true })
          .click();
        await dialog
          .getByLabel("Studio theme", { exact: true })
          .selectOption(scheme);
        await page.waitForFunction(
          (scheme) =>
            document.documentElement.dataset.mantineColorScheme === scheme,
          scheme,
        );
        await page.screenshot({
          path: join(evidence, `settings-${width}-${scheme}.png`),
        });
        await page.keyboard.press("Escape");
        await page.getByRole("dialog").waitFor({ state: "hidden" });
        const geometry = await page.evaluate(() => ({
          width: innerWidth,
          overflow: document.documentElement.scrollWidth,
          controls: [
            ...document.querySelectorAll(".simple-workspace-header button"),
          ]
            .filter((node) => node.getBoundingClientRect().width)
            .map((node) => {
              const rect = node.getBoundingClientRect();
              return {
                label: node.getAttribute("aria-label") || node.textContent,
                left: rect.left,
                right: rect.right,
              };
            }),
        }));
        assert.ok(
          geometry.overflow <= geometry.width,
          JSON.stringify(geometry),
        );
        assert.ok(
          geometry.controls.every(
            (item) => item.left >= 0 && item.right <= width + 1,
          ),
          JSON.stringify(geometry),
        );
        await page.screenshot({
          path: join(evidence, `chat-${width}-${scheme}.png`),
        });
      }
      if (width < 761) {
        await page.locator("#sidebar-toggle").click();
        await page
          .getByRole("button", { name: "Add project", exact: true })
          .click();
        await page
          .getByRole("dialog", { name: "Add project", exact: true })
          .waitFor();
        await page.keyboard.press("Escape");
        await page.keyboard.press("Escape");
      }
    }
    const compactPage = await testBrowser.newPage({
      viewport: { width: 860, height: 960 },
    });
    let warningNoticesEnabled = false;
    compactPage.on("pageerror", (error) => errors.push(error.message));
    await compactPage.route("**/api/sync/identity", (route) =>
      route.fulfill({ status: 404, json: { error: "Fixture without sync" } }),
    );
    await compactPage.route("**/api/state*", async (route) => {
      const state = await route.fetch().then((response) => response.json());
      const usageError = {
        codexErrorInfo: "usageLimitExceeded",
        message: "Usage limit reached",
      };
      const addError = (agent) =>
        agent.name === "Release lead" ? { ...agent, error: usageError } : agent;
      state.threads = state.threads.map(addError);
      state.runtime.agents = state.runtime.agents.map(addError);
      state.runtime.nativeNotices = warningNoticesEnabled
        ? [
            {
              id: "fixture-current-account-warning",
              accountKey: "default",
              nativeNotice: "warning",
              message: "Account configuration needs review",
              details: {
                code: "fixture_configuration",
                files: ["first.toml", "second.toml"],
                retryAllowed: false,
              },
            },
            {
              id: "fixture-other-account-warning",
              accountKey: "other-account",
              nativeNotice: "warning",
              message: "Account configuration needs review",
              details: {
                code: "fixture_configuration",
                files: ["first.toml", "second.toml"],
                retryAllowed: false,
              },
            },
          ]
        : [];
      if (state.nodes) state.nodes = [...state.threads, ...(state.chats || [])];
      return route.fulfill({ json: state });
    });
    await compactPage.route("**/api/limits*", (route) =>
      route.fulfill({
        json: {
          at: Date.now() / 1000,
          data: {
            rateLimits: {
              limitId: "codex",
              primary: {
                usedPercent: 25,
                windowDurationMins: 300,
                resetsAt: Date.now() / 1000 + 3600,
              },
            },
          },
        },
      }),
    );
    await compactPage.goto(origin);
    if (
      (await compactPage
        .locator("#sidebar-toggle")
        .getAttribute("aria-expanded")) === "false"
    )
      await compactPage.locator("#sidebar-toggle").click();
    await compactPage
      .locator(".chat-row")
      .filter({ hasText: "Release lead" })
      .click();
    await compactPage.locator(".native-error").waitFor();
    assert.equal(
      await compactPage.locator(".native-error").count(),
      1,
      "A blocking current-chat error remains visible alongside warning notices",
    );
    assert.equal(
      await compactPage
        .locator('#conversation-header-tools button[aria-label="Warnings"]')
        .count(),
      0,
      "A blocking current-chat error alone does not produce the warning trigger",
    );
    assert.equal(
      await compactPage.locator(".native-account-notices").count(),
      0,
      "Account warning details are not duplicated in the workspace transcript",
    );
    warningNoticesEnabled = true;
    await compactPage.reload();
    await compactPage
      .locator(".chat-row")
      .filter({ hasText: "Release lead" })
      .click();
    const warningTrigger = compactPage.locator(
      '#conversation-header-tools button[aria-label="Warnings"]',
    );
    await warningTrigger.waitFor({ state: "visible" });
    assert.equal(
      await compactPage.locator(".native-account-notices").count(),
      0,
    );
    const warningsDialogPromise = compactPage
      .getByRole("dialog", {
        name: "Warnings",
        exact: true,
      })
      .waitFor();
    await warningTrigger.click();
    await warningsDialogPromise;
    const warningsDialog = compactPage.getByRole("dialog", {
      name: "Warnings",
      exact: true,
    });
    await warningsDialog.getByText("Account notice", { exact: true }).waitFor();
    assert.equal(
      await warningsDialog
        .getByText("Account configuration needs review", {
          exact: true,
        })
        .count(),
      1,
      "The selected account warning appears once in full details",
    );
    assert.equal(
      await warningsDialog.getByText("other-account", { exact: false }).count(),
      0,
      "Notices for other accounts are excluded",
    );
    assert.equal(
      await warningsDialog
        .getByText('"fixture_configuration"', {
          exact: false,
        })
        .count(),
      1,
    );
    await compactPage.waitForTimeout(250);
    await compactPage.screenshot({
      path: join(evidence, "warnings-desktop.png"),
    });
    await compactPage.setViewportSize({ width: 390, height: 844 });
    await compactPage.waitForTimeout(250);
    await compactPage.screenshot({
      path: join(evidence, "warnings-mobile.png"),
    });
    await warningsDialog
      .getByRole("button", { name: "Close", exact: true })
      .click();
    await warningsDialog.waitFor({ state: "hidden" });
    await compactPage.locator(".native-error").waitFor({ state: "visible" });
    await compactPage.screenshot({
      path: join(evidence, "usage-footer-mobile.png"),
    });
    await compactPage.setViewportSize({ width: 1440, height: 960 });
    await compactPage.screenshot({
      path: join(evidence, "usage-footer-desktop.png"),
    });
    await compactPage.setViewportSize({ width: 860, height: 960 });
    await compactPage
      .getByRole("button", { name: "Chat actions", exact: true })
      .click();
    await compactPage
      .getByRole("menuitem", { name: "Team", exact: true })
      .click();
    await compactPage.locator("#team").waitFor();
    await compactPage.waitForFunction(
      () =>
        document.querySelector(".conversation-header-tools-menu")?.dataset
          .compact === "yes",
    );
    const toolsSummary = compactPage.locator(
      '.conversation-header-tools-summary[aria-label="Conversation tools"]',
    );
    await toolsSummary.waitFor();
    const workspaceBounds = await compactPage
      .locator(".workspace")
      .evaluate((element) => {
        const { x, width } = element.getBoundingClientRect();
        return { x, width };
      });
    assert.ok(
      workspaceBounds.width < 900,
      `Sidebar/team should constrain the workspace: ${JSON.stringify(workspaceBounds)}`,
    );
    await compactPage.locator("#team-close").click();
    if (
      (await compactPage
        .locator("#sidebar-toggle")
        .getAttribute("aria-expanded")) === "true"
    )
      await compactPage.locator("#sidebar-toggle").click();
    await compactPage.waitForFunction(
      () =>
        document.querySelector(".workspace").clientWidth < 900 &&
        document.querySelector(".conversation-header-tools-menu")?.dataset
          .compact === "yes",
    );
    await toolsSummary.click();
    await compactPage.waitForFunction(() =>
      document
        .querySelector(".conversation-header-tools-menu")
        ?.hasAttribute("open"),
    );
    assert.equal(
      await compactPage
        .locator("#conversation-header-tools .usage-footer")
        .count(),
      0,
      "Account usage is restored beneath the composer",
    );
    const usage = compactPage.locator("#usage-footer");
    await usage.locator(".session-cost-summary").waitFor({ state: "visible" });
    await usage.getByRole("button", { name: "Chat context" }).waitFor({
      state: "visible",
    });
    await usage.getByRole("button", { name: "Account limits" }).waitFor({
      state: "visible",
    });
    const [usageBounds, composerBounds] = await Promise.all([
      usage.boundingBox(),
      compactPage.locator("#composer").boundingBox(),
    ]);
    assert.ok(usageBounds && composerBounds);
    assert.ok(usageBounds.y >= composerBounds.y + composerBounds.height - 1);
    await compactPage
      .locator(
        "#conversation-header-tools .conversation-prompt-navigation-slot .prompt-navigation-compact",
      )
      .waitFor({ state: "visible" });
    const compactMenuBounds = await compactPage
      .locator(".conversation-header-tools-content")
      .boundingBox();
    assert.ok(compactMenuBounds);
    assert.ok(
      compactMenuBounds.x >= 0 &&
        compactMenuBounds.x + compactMenuBounds.width <= 860 &&
        compactMenuBounds.y >= 0 &&
        compactMenuBounds.y + compactMenuBounds.height <= 960,
      JSON.stringify(compactMenuBounds),
    );
    await compactPage.screenshot({
      path: join(evidence, "constrained-header-tools-menu.png"),
    });
    await toolsSummary.click();
    await compactPage.waitForFunction(
      () =>
        !document
          .querySelector(".conversation-header-tools-menu")
          ?.hasAttribute("open"),
    );
    await compactPage
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    const studioSettings = compactPage.getByTestId("studio-settings");
    assert.deepEqual(
      (await studioSettings.getByRole("tab").allTextContents()).map((label) =>
        label.trim(),
      ),
      ["Accounts", "Appearance", "Hotkeys"],
    );
    await studioSettings
      .getByRole("tab", { name: "Appearance", exact: true })
      .click();
    await studioSettings
      .getByRole("slider", { name: "Main font size" })
      .press("End");
    await compactPage.keyboard.press("Escape");
    await studioSettings.waitFor({ state: "hidden" });
    await compactPage.waitForFunction(
      () =>
        getComputedStyle(document.documentElement).getPropertyValue(
          "--studio-main-font-size",
        ) === "24px" &&
        document.querySelector(".conversation-header-tools-menu")?.dataset
          .compact === "yes",
    );
    await compactPage
      .getByRole("button", { name: "View account limits", exact: true })
      .click();
    const accountLimits = compactPage.getByRole("region", {
      name: "Account limits details",
    });
    await accountLimits.waitFor();
    const limitsBounds = await compactPage
      .locator(".account-limits-popover")
      .boundingBox();
    assert.ok(limitsBounds);
    assert.ok(
      limitsBounds.x >= 0 &&
        limitsBounds.x + limitsBounds.width <= 860 &&
        limitsBounds.y >= 0 &&
        limitsBounds.y + limitsBounds.height <= 960,
      JSON.stringify(limitsBounds),
    );
    await compactPage.screenshot({
      path: join(evidence, "quota-error-open-account-limits.png"),
    });
    await compactPage.close();
    const firstUse = await testBrowser.newPage({
      viewport: { width: 320, height: 844 },
      isMobile: true,
      hasTouch: true,
    });
    firstUse.on("pageerror", (error) => errors.push(error.message));
    const emptyState = {
      ...snapshot,
      stateDir: snapshot.stateDir + "/phone-first",
      threads: [],
      chats: [],
      runtime: {
        ...snapshot.runtime,
        agents: [],
        projects: [],
        rooms: [],
        requests: [],
        complaints: [],
        tasks: [],
        monitors: [],
      },
    };
    await firstUse.route("**/api/sync/identity", (route) =>
      route.fulfill({ status: 404, json: { error: "Fixture without sync" } }),
    );
    await firstUse.route(/\/api\/state(?:\?.*)?$/, (route) =>
      route.fulfill({ json: emptyState }),
    );
    await firstUse.route("**/api/directories*", (route) =>
      route.fulfill({
        json: { path: "/workspace/phone-first", directories: [] },
      }),
    );
    const creations = [],
      projects = [];
    let createdResolve;
    const createdRequest = new Promise((resolve) => {
      createdResolve = resolve;
    });
    await firstUse.route("**/api/projects", (route) => {
      projects.push(route.request().postDataJSON());
      return route.fulfill({ json: {} });
    });
    await firstUse.route("**/api/leads", (route) => {
      const body = route.request().postDataJSON();
      creations.push(body);
      createdResolve();
      return route.fulfill({ json: { id: body.id } });
    });
    await firstUse.goto(origin);
    await firstUse.locator("#sidebar-toggle").click();
    await firstUse
      .getByRole("button", { name: "New chat", exact: true })
      .click();
    const folderDialog = firstUse.getByRole("dialog", {
      name: "Choose project folder",
      exact: true,
    });
    await folderDialog
      .getByRole("button", { name: "Use this folder", exact: true })
      .click();
    await createdRequest;
    assert.equal(projects.length, 1);
    assert.equal(creations.length, 1);
    assert.equal(creations[0].cwd, "/workspace/phone-first");
    assert.ok(creations[0].id);
    await firstUse.close();
    const branchPage = await testBrowser.newPage({
      viewport: { width: 390, height: 844 },
      isMobile: true,
      hasTouch: true,
    });
    branchPage.on("pageerror", (error) => errors.push(error.message));
    const sourceAgent = {
      ...snapshot.threads.find((item) => item.name === "Other project"),
      status: "idle",
      inFlight: false,
      canSend: true,
      threadId: "fixture-source-thread",
      turnId: undefined,
    };
    let branchAgent,
      showBranch = false;
    const branchState = () => ({
      ...snapshot,
      stateDir: snapshot.stateDir + "/branch",
      threads: showBranch ? [sourceAgent, branchAgent] : [sourceAgent],
      chats: [],
      runtime: {
        ...snapshot.runtime,
        agents: showBranch ? [sourceAgent, branchAgent] : [sourceAgent],
        rooms: [],
        requests: [],
        complaints: [],
        tasks: [],
        monitors: [],
      },
    });
    await branchPage.route("**/api/sync/identity", (route) =>
      route.fulfill({ status: 404, json: { error: "Fixture without sync" } }),
    );
    await branchPage.route(/\/api\/state(?:\?.*)?$/, (route) =>
      route.fulfill({ json: branchState() }),
    );
    await branchPage.route("**/api/transcript/stream*", (route) =>
      route.fulfill({ status: 503, json: { error: "Use fixture polling" } }),
    );
    await branchPage.route("**/api/transcript?*", (route) => {
      const id = new URL(route.request().url()).searchParams.get("id");
      return route.fulfill({
        json: {
          items:
            id === sourceAgent.id
              ? [
                  {
                    id: "branch-source-message",
                    role: "user",
                    text: "Original branch prompt",
                    turnId: "completed-turn",
                    turnStatus: "completed",
                    at: 1,
                  },
                  {
                    id: "branch-source-answer",
                    role: "assistant",
                    text: "Completed branch answer",
                    turnId: "completed-turn",
                    turnStatus: "completed",
                    at: 2,
                  },
                ]
              : [],
          agent: { id, status: "idle" },
          historyVersion: "branch-fixture",
        },
      });
    });
    await branchPage.route("**/api/branch", (route) => {
      const body = route.request().postDataJSON();
      branchAgent = {
        ...sourceAgent,
        id: body.id,
        rootId: body.id,
        name: "Reviewed branch",
        threadId: "fixture-new-thread",
      };
      return route.fulfill({
        json: {
          agent: branchAgent,
          draft: { text: "Original branch prompt", assets: [] },
        },
      });
    });
    await branchPage.goto(origin);
    const answerActions = branchPage.locator(
      '[data-message="branch-source-answer"] .message-bottom',
    );
    const actionLabels = [
      ["Copy message", "Copy this message"],
      ["Quote message", "Quote this message in your reply"],
      ["Branch after this turn", "Start a new branch after this turn"],
      [
        "Another answer in a new chat",
        "Prepare a request for another answer in a new chat",
      ],
    ];
    for (const [buttonLabel, tooltipLabel] of actionLabels) {
      const action = answerActions.getByRole("button", {
        name: buttonLabel,
        exact: true,
      });
      const tooltip = branchPage.getByRole("tooltip", {
        name: tooltipLabel,
        exact: true,
      });
      await action.hover();
      await tooltip.waitFor({ state: "visible" });
      await branchPage.mouse.move(0, 0);
      await action.focus();
      await tooltip.waitFor({ state: "visible" });
    }
    await branchPage.locator("#message").fill("Preserve source draft");
    await branchPage
      .getByRole("button", { name: "Edit in a new chat", exact: true })
      .click();
    await branchPage
      .getByRole("textbox", { name: "Draft for the new chat", exact: true })
      .fill("Reviewed mobile branch draft");
    await branchPage
      .getByRole("button", { name: "Create draft in new chat", exact: true })
      .click();
    await branchPage.waitForFunction(
      () =>
        document.querySelector("#message")?.value ===
        "Reviewed mobile branch draft",
    );
    await branchPage.waitForTimeout(1800);
    assert.equal(
      await branchPage.locator("#message").inputValue(),
      "Reviewed mobile branch draft",
      "new branch stays selected before the projection arrives",
    );
    showBranch = true;
    await branchPage.waitForFunction(
      () =>
        document.querySelector("#conversation-title")?.textContent ===
        "Reviewed branch",
    );
    assert.equal(
      await branchPage.evaluate((id) => {
        const workspace = JSON.parse(
          localStorage.getItem("codex-sync-workspace") || '"unassigned"',
        );
        return JSON.parse(
          localStorage.getItem(
            `codex-chat-draft:${workspace}:${encodeURIComponent(id)}`,
          ),
        ).text;
      }, sourceAgent.id),
      "Preserve source draft",
    );
    await branchPage.close();
    assert.deepEqual(errors, []);
    console.log(JSON.stringify({ result: "PASS", evidence }));
  } finally {
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
  }
});
