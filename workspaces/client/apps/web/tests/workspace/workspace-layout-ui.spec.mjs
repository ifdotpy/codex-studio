import {
  readTestState,
  stubSessionCost,
  test,
  spawnFixture as spawn,
} from "../playwright.mjs";
// Production workspace geometry with long labels and an active agent panel.
// The fixture uses an isolated runtime. No model service or user state is used.
import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

async function runWorkspaceLayout(
  { browser: testBrowser, page: runnerPage, context: runnerContext },
  viewports,
  assertTranscriptShare,
) {
  test.setTimeout(60_000);
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const root = await mkdtemp(join(tmpdir(), "codex-workspace-layout-"));
  const fixture = spawn(
    "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    { cwd: repo, stdio: ["pipe", "pipe", "pipe"] },
  );
  let closing = false;
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += chunk));

  const longTitle =
    "A very long conversation title that tests header compression at every supported viewport width";
  const longProject =
    "/Users/igor/Projects/a-super-long-project-name-for-layout-validation-with-many-words";
  const longProjectName = longProject.split("/").at(-1);
  const longAccount =
    "account-owner-with-a-very-long-email-address-for-layout@example.test";
  const longAccountLabel = "Account owner with a very long label";
  const panelHeight = 150;
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(Error(log || "Fixture exited early")));
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await readTestState(origin);
  const fixtureAccounts = await (await fetch(origin + "/api/accounts")).json();
  const lead = initial.threads.find((agent) => agent.name === "Release lead");
  assert.ok(lead, "fixture has a lead chat");

  const panel = {
    agent: lead.id,
    format: "markdown",
    markdown:
      "Active agent progress\n\nPanel content stays inside the shared reading column.",
    path: "/fixture/PROGRESS.md",
    revision: "fixture-progress",
    exists: true,
    error: null,
  };

  try {
    const page = runnerPage;
    await page.setViewportSize(viewports[0]);
    await stubSessionCost(page);
    await page.addInitScript(() => {
      localStorage.setItem(
        "codex-studio-preferences-v1",
        JSON.stringify({
          theme: "auto",
          sidebarFontSize: 14,
          mainFontSize: 14,
          fontFamily: "system",
          contentWidth: 60,
          sidebarShortcut: "Control+b",
        }),
      );
    });
    if (process.env.STUDIO_LAYOUT_ASSETS) {
      await page.route("**/*", async (route) => {
        const url = new URL(route.request().url());
        if (url.origin !== origin || url.pathname.startsWith("/api/"))
          return route.fallback();
        const relative =
          url.pathname === "/" ? "index.html" : url.pathname.slice(1);
        if (relative.includes("..")) throw Error("Invalid artifact path");
        const file = join(process.env.STUDIO_LAYOUT_ASSETS, relative);
        const contentType = relative.endsWith(".js")
          ? "text/javascript"
          : relative.endsWith(".css")
            ? "text/css"
            : relative.endsWith(".html")
              ? "text/html"
              : relative.endsWith(".json")
                ? "application/json"
                : undefined;
        await route.fulfill({
          body: await readFile(file),
          ...(contentType ? { contentType } : {}),
        });
      });
    }
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));

    await page.route("**/api/sync/pull?**", async (route) => {
      try {
        const response = await route.fetch();
        const data = await response.json();
        const scope = new URL(route.request().url()).searchParams.get("scope");
        if (scope === "state:entities:v1") {
          for (const document of data.documents || []) {
            const entity = JSON.parse(document.payload);
            if (entity.collection !== "agent") continue;
            if (entity.id === lead.id) {
              entity.value.name = longTitle;
              entity.value.cwd = longProject;
            } else if (entity.value.rootId === lead.id) {
              entity.value.cwd = longProject;
            }
            document.payload = JSON.stringify(entity);
          }
        }
        await route.fulfill({ response, json: data });
      } catch (error) {
        if (!closing) throw error;
      }
    });
    await page.route("**/api/accounts", async (route) => {
      const data = structuredClone(fixtureAccounts);
      const account = data.accounts.find((item) => item.id === "default");
      account.email = longAccount;
      account.label = "Account owner with a very long label";
      await route.fulfill({ json: data });
    });
    await page.route("**/api/panel?**", (route) =>
      route.fulfill({ json: panel }),
    );

    await page.goto(origin);
    await page.locator("[data-chat]").filter({ hasText: longTitle }).click();
    await page.getByText("I assigned 40 workers", { exact: false }).waitFor();
    await page.locator(".agent-panel[data-fit='yes']").waitFor();

    const measure = () =>
      page.evaluate(() => {
        const box = (selector) => {
          const element = document.querySelector(selector);
          if (!element) return null;
          const rect = element.getBoundingClientRect();
          const style = getComputedStyle(element);
          return {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            right: rect.right,
            bottom: rect.bottom,
            clientWidth: element.clientWidth,
            scrollWidth: element.scrollWidth,
            clientHeight: element.clientHeight,
            scrollHeight: element.scrollHeight,
            minHeight: style.minHeight,
            computedHeight: style.height,
            lineHeight: style.lineHeight,
            paddingTop: Number.parseFloat(style.paddingTop) || 0,
            paddingBottom: Number.parseFloat(style.paddingBottom) || 0,
            borderTop: Number.parseFloat(style.borderTopWidth) || 0,
            borderBottom: Number.parseFloat(style.borderBottomWidth) || 0,
            paddingLeft: Number.parseFloat(style.paddingLeft) || 0,
            paddingRight: Number.parseFloat(style.paddingRight) || 0,
            marginTop: style.marginTop,
            text: element.textContent?.trim() || "",
          };
        };
        const messages = box("#messages");
        const messageContent = box(".message-content");
        const transcript = messageContent
          ? { left: messageContent.x, right: messageContent.right }
          : null;
        const header = document.querySelector(".workspace-header");
        const headerRect = header?.getBoundingClientRect();
        const headerChildren = header
          ? [...header.querySelectorAll(":scope > *")]
              .filter((element) => getComputedStyle(element).display !== "none")
              .map((element) => {
                const rect = element.getBoundingClientRect();
                return { left: rect.left, right: rect.right };
              })
          : [];
        return {
          viewport: { width: innerWidth, height: innerHeight },
          document: {
            scrollWidth: document.documentElement.scrollWidth,
            clientWidth: document.documentElement.clientWidth,
            bodyScrollWidth: document.body.scrollWidth,
          },
          header: {
            ...box(".workspace-header"),
            overflow: header ? header.scrollWidth - header.clientWidth : 0,
            children: headerChildren,
            left: headerRect?.left || 0,
            right: headerRect?.right || 0,
          },
          title: box("#conversation-title"),
          heading: box(".conversation-heading"),
          account: box(".setup-account[data-selected]"),
          project: box("#project"),
          settings: box(".chat-settings-panel"),
          settingsModal: box(".mantine-Modal-content"),
          messages,
          transcript,
          panel: box(".agent-panel"),
          composer: box("#composer"),
          composerContext: box(".composer-context-row"),
          message: {
            ...box("#message"),
            rows: document.querySelector("#message")?.rows,
          },
          usage: box("#usage-footer"),
          shortcuts: box(".workspace-shortcuts"),
        };
      });

    const measurements = [];
    for (const viewport of viewports) {
      await page.setViewportSize(viewport);
      await page.waitForTimeout(80);
      const current = await measure();
      measurements.push(current);
      console.log("Workspace viewport geometry", {
        viewport: current.viewport,
        textarea: current.message.height,
        body: current.composer.height - (current.composerContext?.height || 0),
        context: current.composerContext?.height || 0,
        composer: current.composer.height,
        transcript: current.messages.height,
        transcriptShare: current.messages.height / viewport.height,
      });
      await writeFile(
        join(root, "measurements.json"),
        JSON.stringify(measurements, null, 2),
      );
      await writeFile(
        join(root, "measurements.json"),
        JSON.stringify(measurements, null, 2),
      );
      const prefix = viewport.name;
      assert.equal(
        await page.locator("#conversation-title").getAttribute("title"),
        longTitle,
        `${prefix}: full title is available to assistive and hover users`,
      );
      assert.ok(
        current.title?.width >= (viewport.width <= 760 ? 132 : 180),
        `${prefix}: title width ${current.title?.width}`,
      );
      assert.ok(
        current.document.scrollWidth <= current.viewport.width + 1 &&
          current.document.bodyScrollWidth <= current.viewport.width + 1,
        `${prefix}: document horizontal overflow`,
      );
      assert.ok(
        current.header.overflow <= 1 &&
          current.header.children.every(
            ({ left, right }) =>
              left >= current.header.left - 1 &&
              right <= current.header.right + 1,
          ),
        `${prefix}: header horizontal overflow`,
      );
      assertTranscriptShare(current, viewport);
      assert.equal(
        current.message.rows,
        2,
        `${prefix}: composer keeps the intended two-row textarea`,
      );
      // Textarea line boxes are laid out in whole CSS pixels even though the
      // computed line-height can be fractional. Derive the two-row box from
      // the browser's computed style instead of duplicating font breakpoints.
      const lineBox = Math.floor(Number.parseFloat(current.message.lineHeight));
      const designedTextareaHeight =
        2 * lineBox +
        current.message.paddingTop +
        current.message.paddingBottom +
        current.message.borderTop +
        current.message.borderBottom;
      assert.ok(
        Number.parseFloat(current.message.minHeight) <= 36 &&
          Math.abs(current.message.height - designedTextareaHeight) <= 0.5,
        `${prefix}: two-row textarea height ${current.message.height}; computed style resolves to ${designedTextareaHeight}px (${current.message.lineHeight}, ${current.message.minHeight})`,
      );
      // The established one-row body caps grow by exactly one prompt line
      // because 9ff09e46 fixed the textarea at two rows: 90 + 14×1.65 =
      // 113.1px desktop; 110 + 16×1.65 = 136.4px mobile.
      const compactComposerLimit =
        (viewport.width <= 760 ? 110 : 90) +
        (viewport.width <= 760 ? 16 : 14) * 1.65;
      const composerContextHeight = current.composerContext?.height || 0;
      assert.ok(composerContextHeight <= 80, `${prefix}: context row height`);
      if (viewport.width > 760) {
        const composerBodyHeight =
          current.composer.height - composerContextHeight;
        assert.ok(
          composerBodyHeight <= compactComposerLimit + 1,
          `${prefix}: blank composer body ${composerBodyHeight}px stays within ${compactComposerLimit}px`,
        );
      }
      assert.equal(
        await page.locator("#messages #requests").count(),
        1,
        `${prefix}: requests scroll with the transcript`,
      );
      assert.equal(
        await page.locator("#conversation > .user-tasks-compact").count(),
        0,
        `${prefix}: tasks do not reserve transcript space`,
      );
      const headerTools = page.locator(
        "#conversation-header-tools .conversation-header-tools-menu",
      );
      assert.equal(
        await page.locator("#conversation-header-tools .usage-footer").count(),
        0,
        `${prefix}: usage does not occupy the header tools`,
      );
      const promptNavigation = headerTools.locator(
        ".conversation-prompt-navigation-slot .prompt-navigation-compact",
      );
      assert.equal(
        await promptNavigation.count(),
        1,
        `${prefix}: prompt history uses the header tools portal`,
      );
      if ((await headerTools.getAttribute("data-compact")) === "yes") {
        await headerTools.locator(".conversation-header-tools-summary").click();
        await promptNavigation.waitFor({ state: "visible" });
        const toolsContent = headerTools.locator(
          ".conversation-header-tools-content",
        );
        const contentBox = await toolsContent.boundingBox();
        assert.ok(
          contentBox &&
            contentBox.x >= 0 &&
            contentBox.x + contentBox.width <= viewport.width + 1,
          `${prefix}: compact prompt and usage controls fit the viewport`,
        );
        await headerTools.locator(".conversation-header-tools-summary").click();
      } else {
        await promptNavigation.waitFor({ state: "visible" });
      }
      if (current.usage) {
        const usage = page.locator("#usage-footer");
        await usage.waitFor({ state: "visible" });
        await usage.locator(".session-cost-summary").waitFor({
          state: "visible",
          timeout: 5_000,
        });
        await usage.getByRole("button", { name: "Chat context" }).waitFor({
          state: "visible",
        });
        await usage
          .getByRole("button", { name: "Account limits", exact: true })
          .waitFor({ state: "visible" });
        const composerBounds = await page.locator("#composer").boundingBox();
        const usageBounds = await usage.boundingBox();
        assert.ok(composerBounds && usageBounds);
        assert.ok(
          usageBounds.y >= composerBounds.y + composerBounds.height - 1,
          `${prefix}: context, cost, and limits appear beneath the composer`,
        );
      }
      assert.ok(current.panel, `${prefix}: active panel is present`);
      assert.equal(
        current.panel.marginTop,
        "8px",
        `${prefix}: progress panel starts 8px below its preceding content`,
      );
      assert.ok(
        current.panel.height > 0 && current.panel.height <= panelHeight + 1,
        `${prefix}: panel height ${current.panel.height}`,
      );
      assert.equal(
        await page.locator(".agent-panel iframe").count(),
        0,
        `${prefix}: Markdown progress does not use a retired panel frame`,
      );
      const transcriptWidth =
        current.transcript.right - current.transcript.left;
      const expectedTranscriptWidth =
        viewport.width <= 760
          ? current.messages.width - 28
          : (current.messages.width - 48) * 0.6;
      assert.ok(
        transcriptWidth <= expectedTranscriptWidth + 1,
        `${prefix}: readable text width ${transcriptWidth}`,
      );
      assert.ok(
        Math.abs(
          current.transcript.left +
            current.transcript.right -
            current.composer.x -
            current.composer.right,
        ) <= 2,
        `${prefix}: text stays centered above the composer`,
      );
      assert.ok(
        Math.abs(current.panel.x - current.composer.x) <= 1 &&
          Math.abs(current.panel.right - current.composer.right) <= 1,
        `${prefix}: panel and composer share their edges`,
      );
      const directSettings = page.getByRole("button", {
        name: "Chat settings",
        exact: true,
      });
      if (await directSettings.isVisible()) await directSettings.click();
      else {
        await page
          .getByRole("button", { name: "Chat actions", exact: true })
          .click();
        await page
          .getByRole("menuitem", { name: "Chat settings", exact: true })
          .click();
      }
      const settingsModal = page.getByRole("dialog", {
        name: "Chat settings",
        exact: true,
      });
      await settingsModal
        .getByRole("button", { name: "Main agent settings", exact: true })
        .click();
      const mainAgentSettings = page.getByRole("dialog", {
        name: "Main agent settings",
        exact: true,
      });
      const selectedAccount = mainAgentSettings.getByRole("button", {
        name: `Select account ${longAccountLabel}`,
        exact: true,
      });
      await selectedAccount.waitFor();
      await mainAgentSettings.evaluate(
        (element) =>
          new Promise((resolve, reject) => {
            let previousBounds;
            const timeout = window.setTimeout(
              () =>
                reject(
                  new Error("Main agent settings transition did not settle"),
                ),
              10_000,
            );
            const sample = () => {
              const rect = element.getBoundingClientRect();
              const bounds = [rect.x, rect.y, rect.width, rect.height];
              const sameBounds =
                previousBounds &&
                bounds.every((value, index) => value === previousBounds[index]);
              const animationsFinished = element
                .getAnimations({ subtree: true })
                .every(
                  (animation) =>
                    animation.playState !== "running" &&
                    animation.playState !== "pending",
                );
              if (sameBounds && animationsFinished) {
                window.clearTimeout(timeout);
                resolve(true);
                return;
              }
              previousBounds = bounds;
              window.requestAnimationFrame(sample);
            };
            window.requestAnimationFrame(sample);
          }),
      );
      const executionSettingsGeometry = await mainAgentSettings.evaluate(
        (element) => {
          const rect = element.getBoundingClientRect();
          const style = getComputedStyle(element);
          return {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            bottom: rect.bottom,
            scrollHeight: element.scrollHeight,
            clientHeight: element.clientHeight,
            overflowY: style.overflowY,
          };
        },
      );
      console.log(
        `${prefix}: nested execution settings geometry`,
        executionSettingsGeometry,
        await selectedAccount.boundingBox(),
      );
      const settings = await measure();
      let projectGeometry;
      if (viewport.width > 760) {
        const projectButton = page.getByRole("button", {
          name: longProjectName,
          exact: true,
        });
        await projectButton.waitFor({ state: "visible" });
        projectGeometry = await projectButton.evaluate((element) => {
          const rect = element.getBoundingClientRect();
          const sidebar = document.querySelector("#sidebar");
          const sidebarRect = sidebar?.getBoundingClientRect();
          const label = element.querySelector("span");
          return {
            rect: { x: rect.x, right: rect.right },
            sidebar: sidebarRect
              ? { x: sidebarRect.x, right: sidebarRect.right }
              : null,
            label: label
              ? {
                  scrollWidth: label.scrollWidth,
                  clientWidth: label.clientWidth,
                  textOverflow: getComputedStyle(label).textOverflow,
                }
              : null,
            title: element.getAttribute("title"),
            text: element.innerText,
          };
        });
        assert.equal(projectGeometry.text, longProjectName);
        assert.equal(projectGeometry.title, longProject);
        assert.equal(projectGeometry.label.textOverflow, "ellipsis");
        assert.ok(
          projectGeometry.label.scrollWidth > projectGeometry.label.clientWidth,
          `${prefix}: long project label is clipped within its sidebar label`,
        );
        assert.ok(
          projectGeometry.rect.x >= projectGeometry.sidebar.x - 1 &&
            projectGeometry.rect.right <= projectGeometry.sidebar.right + 1,
          `${prefix}: project control fits the sidebar ${JSON.stringify(projectGeometry)}`,
        );
      }
      current.chatSettings = {
        modal: settings.settingsModal,
        panel: settings.settings,
        account: settings.account,
        project: projectGeometry,
      };
      assert.equal(
        settings.account?.text,
        longAccountLabel,
        `${prefix}: account label`,
      );
      assert.equal(
        await selectedAccount.getAttribute("aria-label"),
        `Select account ${longAccountLabel}`,
      );
      assert.ok(
        settings.settingsModal.x >= -1 &&
          settings.settingsModal.right <= viewport.width + 1 &&
          settings.settingsModal.scrollWidth <=
            settings.settingsModal.clientWidth + 1 &&
          settings.settings.scrollWidth <= settings.settings.clientWidth + 1,
        `${prefix}: settings with long labels fit the viewport ${JSON.stringify({ modal: settings.settingsModal, panel: settings.settings })}`,
      );
      await page.keyboard.press("Escape");
      await mainAgentSettings.waitFor({ state: "hidden" });
      assert.ok(
        settings.account.x >= executionSettingsGeometry.x - 1 &&
          settings.account.right <=
            executionSettingsGeometry.x + executionSettingsGeometry.width + 1,
        `${prefix}: account control fits the Main agent settings dialog ${JSON.stringify({ account: settings.account, dialog: executionSettingsGeometry })}`,
      );
      await settingsModal
        .getByRole("button", { name: "Close", exact: true })
        .click();
      await settingsModal.waitFor({ state: "hidden" });
      if (viewport.width === 1024) {
        await page
          .getByRole("button", { name: "Chat actions", exact: true })
          .click();
        await page.getByRole("menuitem", { name: /^Team/ }).click();
        const drawer = page.locator(".mantine-Drawer-content:visible");
        await drawer.waitFor();
        await page.waitForTimeout(300);
        const drawerBox = await drawer.boundingBox();
        assert.ok(drawerBox, "team drawer has geometry");
        assert.ok(
          drawerBox.x >= -1 &&
            drawerBox.x + drawerBox.width <= viewport.width + 1,
          "team drawer stays inside the viewport",
        );
        const open = await measure();
        assert.ok(
          open.document.scrollWidth <= viewport.width + 1 &&
            open.header.overflow <= 1,
          "open team drawer does not create horizontal overflow",
        );
        await page.keyboard.press("Escape");
        await drawer.waitFor({ state: "hidden" });
      }
    }

    await writeFile(
      join(root, "workspace-layout.json"),
      JSON.stringify(measurements, null, 2),
    );
    console.log(
      "Workspace layout measurements",
      measurements.map((measurement) => ({
        viewport: measurement.viewport,
        transcriptHeight: measurement.messages.height,
        composerHeight: measurement.composer.height,
        contextHeight: measurement.composerContext?.height || 0,
        textareaHeight: measurement.message.height,
        textareaRows: measurement.message.rows,
      })),
    );
    assert.deepEqual(errors, []);
    console.log(
      `PASS workspace layout: long title, project, account, capped panel, narrow text, aligned composer, blank composer, and team drawer for ${viewports.map(({ width }) => `${width}px`).join(", ")}. Evidence ${root}`,
    );
  } catch (error) {
    console.error("Layout evidence:", root);
    throw error;
  } finally {
    closing = true;
    for (const context of testBrowser.contexts()) {
      for (const page of context.pages())
        await page.unrouteAll({ behavior: "wait" });
    }
    await Promise.all(
      testBrowser
        .contexts()
        .filter((ownedContext) => ownedContext !== runnerContext)
        .map((ownedContext) => ownedContext.close()),
    );
  }
}

const workspaceViewports = [
  { width: 1440, height: 900, name: "desktop-1440x900" },
  { width: 1280, height: 800, name: "desktop-1280x800" },
  { width: 390, height: 844, name: "mobile-390x844" },
];

test("Workspace layout ui", async ({ browser, page, context }) =>
  runWorkspaceLayout(
    { browser, page, context },
    workspaceViewports,
    (current, viewport) => {
      assert.ok(
        current.messages.height >= viewport.height * 0.48,
        `${viewport.name}: transcript height ${current.messages.height} stays at least 48% of viewport height`,
      );
    },
  ));

test("workspace transcript retains 48% at 1024×768", async ({
  browser,
  page,
  context,
}) =>
  runWorkspaceLayout(
    { browser, page, context },
    [{ width: 1024, height: 768, name: "desktop-1024x768" }],
    (current, viewport) => {
      test.fail(
        true,
        "The fixed two-row prompt leaves the transcript below its 48% height guarantee at 1024×768; see c4-shell-known-failures.md.",
      );
      assert.ok(
        current.messages.height >= viewport.height * 0.48,
        `${viewport.name}: transcript height ${current.messages.height} stays at least 48% of viewport height`,
      );
    },
  ));

async function measureMobileWorkspaceComposerBody(page) {
  const repo = fileURLToPath(new URL("../../../../../../", import.meta.url));
  const root = await mkdtemp(join(tmpdir(), "codex-workspace-composer-"));
  const fixture = spawn(
    "python3",
    [
      "-B",
      join(repo, "workspaces/runtime/apps/server/tests/simple-ui-fixture.py"),
      root,
    ],
    { cwd: repo, stdio: ["pipe", "pipe", "pipe"] },
  );
  let log = "";
  fixture.stderr.on("data", (chunk) => (log += chunk));
  try {
    const port = await new Promise((resolve, reject) => {
      fixture.stdout.once("data", (chunk) =>
        resolve(Number(String(chunk).trim())),
      );
      fixture.once("exit", () => reject(Error(log || "Fixture exited early")));
    });
    const origin = `http://127.0.0.1:${port}`;
    const initial = await readTestState(origin);
    const fixtureAccounts = await (
      await fetch(origin + "/api/accounts")
    ).json();
    const lead = initial.threads.find((agent) => agent.name === "Release lead");
    assert.ok(lead, "fixture has a lead chat");
    const longTitle =
      "A very long conversation title that tests header compression at every supported viewport width";
    const longProject =
      "/Users/igor/Projects/a-super-long-project-name-for-layout-validation-with-many-words";
    const longAccount =
      "account-owner-with-a-very-long-email-address-for-layout@example.test";

    await stubSessionCost(page);
    await page.route("**/api/sync/pull?**", async (route) => {
      const response = await route.fetch();
      const data = await response.json();
      const scope = new URL(route.request().url()).searchParams.get("scope");
      if (scope === "state:entities:v1") {
        for (const document of data.documents || []) {
          const entity = JSON.parse(document.payload);
          if (entity.collection !== "agent") continue;
          if (entity.id === lead.id) {
            entity.value.name = longTitle;
            entity.value.cwd = longProject;
          } else if (entity.value.rootId === lead.id) {
            entity.value.cwd = longProject;
          }
          document.payload = JSON.stringify(entity);
        }
      }
      await route.fulfill({ response, json: data });
    });
    await page.route("**/api/accounts", async (route) => {
      const data = structuredClone(fixtureAccounts);
      const account = data.accounts.find((item) => item.id === "default");
      account.email = longAccount;
      account.label = "Account owner with a very long label";
      await route.fulfill({ json: data });
    });

    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto(origin);
    await page.locator("[data-chat]").filter({ hasText: longTitle }).click();
    await page.setViewportSize({ width: 390, height: 844 });
    const composer = page.locator("#composer");
    await composer.waitFor({ state: "visible" });
    const contextRow = page.locator(".composer-context-row");
    await contextRow.waitFor({ state: "visible" });
    const bounds = await page.evaluate(() => {
      const composer = document.querySelector("#composer");
      const context = document.querySelector(".composer-context-row");
      return {
        composer: composer?.getBoundingClientRect().height ?? 0,
        context: context?.getBoundingClientRect().height ?? 0,
      };
    });
    const screenshot = test.info().outputPath("composer-390x844.png");
    await page.screenshot({ path: screenshot, animations: "disabled" });
    console.log(`Mobile composer screenshot: ${screenshot}`);
    return {
      body: bounds.composer - bounds.context,
      total: bounds.composer,
      context: bounds.context,
    };
  } finally {
    if (!page.isClosed()) await page.unrouteAll({ behavior: "wait" });
    fixture.kill("SIGTERM");
    if (fixture.exitCode === null && fixture.signalCode === null)
      await new Promise((resolve) => fixture.once("exit", resolve));
  }
}

test("mobile workspace composer setup reaches compact geometry", async ({
  page,
}) => {
  const measured = await measureMobileWorkspaceComposerBody(page);
  assert.ok(measured.total > measured.context);
  assert.ok(measured.body > 0);
});

test("mobile workspace composer body fits its compact height budget", async ({
  page,
}) => {
  const measured = await measureMobileWorkspaceComposerBody(page);
  // The two-row input adds one mobile line (16px × 1.65) to the old 110px cap.
  const mobileComposerBodyCap = 110 + 16 * 1.65;
  assert.ok(
    measured.body <= mobileComposerBodyCap + 1,
    `390×844 composer body stays within ${mobileComposerBodyCap}px: ${measured.body}px`,
  );
});
