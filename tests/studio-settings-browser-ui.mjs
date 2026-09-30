#!/usr/bin/env node
// Browser-local Studio preferences. Uses the production bundle and isolated fixture.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(root, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-settings-browser-"));
const fixture = spawn(
  "python3",
  ["-B", join(root, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
const storageKey = "codex-studio-preferences-v1";
let browser,
  fixtureLog = "";
fixture.stderr.on("data", (chunk) => {
  fixtureLog += chunk;
});

try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (chunk) =>
      resolve(Number(String(chunk).trim())),
    );
    fixture.once("exit", () => reject(new Error(fixtureLog)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const snapshot = await (await fetch(`${origin}/api/state`)).json();
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });

  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  page.setDefaultTimeout(12000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route(/\/api\/panel\?/, (route) => {
    const agent = new URL(route.request().url()).searchParams.get("agent");
    return route.fulfill({
      json: {
        agent,
        format: "markdown",
        markdown: "Short progress text for shared-width alignment.",
        path: "/fixture/PROGRESS.md",
        revision: "studio-settings-browser-test",
      },
    });
  });
  await page.route("**/api/panel/layout", (route) =>
    route.fulfill({ json: {} }),
  );
  await page.addInitScript((key) => {
    localStorage.setItem(key, '{"theme":"dark","sidebarFontSize":99}');
  }, storageKey);
  await page.goto(origin);
  await page.locator("#message").waitFor();
  await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
  await page
    .locator(
      '.agent-panel[aria-label="Agent progress"][data-fit="yes"] .agent-panel-current.progress-markdown',
    )
    .waitFor();

  const openStudioSettings = async () => {
    await page
      .getByRole("button", { name: "Studio settings", exact: true })
      .click();
    return page.getByRole("dialog", {
      name: "Studio settings",
      exact: true,
    });
  };
  const settings = await openStudioSettings();
  await settings
    .getByRole("alert")
    .getByText(
      "Saved Studio preferences were invalid; safe defaults are active.",
      {
        exact: true,
      },
    )
    .waitFor();
  const sidebarSize = settings.getByRole("slider", {
    name: "Sidebar font size",
  });
  const mainSize = settings.getByRole("slider", { name: "Main font size" });
  assert.equal(await sidebarSize.getAttribute("aria-valuenow"), "14");
  assert.equal(await mainSize.getAttribute("aria-valuenow"), "14");

  await sidebarSize.press("End");
  assert.equal(await sidebarSize.getAttribute("aria-valuenow"), "24");
  assert.equal(
    await page
      .locator(".chat-row")
      .first()
      .evaluate((element) => getComputedStyle(element).fontSize),
    "24px",
    "Visible sidebar chat labels follow the maximum sidebar font size",
  );
  await sidebarSize.press("Home");
  assert.equal(await sidebarSize.getAttribute("aria-valuenow"), "12");
  assert.equal(
    await page
      .locator(".chat-row")
      .first()
      .evaluate((element) => getComputedStyle(element).fontSize),
    "12px",
  );
  await mainSize.press("Home");
  assert.equal(await mainSize.getAttribute("aria-valuenow"), "12");
  assert.equal(
    await page
      .locator("#messages .prose")
      .first()
      .evaluate((element) => getComputedStyle(element).fontSize),
    "12px",
  );
  await mainSize.press("End");
  assert.equal(await mainSize.getAttribute("aria-valuenow"), "24");
  assert.equal(
    await page
      .locator("#messages .prose")
      .first()
      .evaluate((element) => getComputedStyle(element).fontSize),
    "24px",
    "Visible transcript text follows the maximum main font size",
  );
  assert.deepEqual(
    await page.evaluate(() => ({
      sidebar: getComputedStyle(document.documentElement).getPropertyValue(
        "--studio-sidebar-font-size",
      ),
      main: getComputedStyle(document.documentElement).getPropertyValue(
        "--studio-main-font-size",
      ),
    })),
    { sidebar: "12px", main: "24px" },
    "Sidebar and main font sizes are independent",
  );

  await settings
    .getByLabel("Studio font family", { exact: true })
    .selectOption("georgia");
  assert.match(
    await page.evaluate(() =>
      getComputedStyle(document.documentElement).getPropertyValue(
        "--studio-font-family",
      ),
    ),
    /Georgia/,
  );
  assert.match(
    await page.evaluate(() => {
      const code = document.createElement("code");
      code.textContent = "const answer = 42";
      document.querySelector("#conversation").append(code);
      const family = getComputedStyle(code).fontFamily;
      code.remove();
      return family;
    }),
    /monospace/i,
    "Code remains monospace when the local UI font changes",
  );

  const theme = settings.getByLabel("Studio theme", { exact: true });
  await page.emulateMedia({ colorScheme: "dark" });
  await theme.selectOption("auto");
  await page.waitForFunction(
    () => document.documentElement.dataset.mantineColorScheme === "dark",
  );
  await theme.selectOption("light");
  await page.waitForFunction(
    () => document.documentElement.dataset.mantineColorScheme === "light",
  );
  await theme.selectOption("dark");
  await page.waitForFunction(
    () => document.documentElement.dataset.mantineColorScheme === "dark",
  );

  const shortcut = settings.getByLabel("Toggle sidebar shortcut", {
    exact: true,
  });
  const defaultShortcut = await shortcut.inputValue();
  await shortcut.focus();
  await page.keyboard.press("Control+k");
  await settings
    .getByRole("status")
    .filter({ hasText: "Browser-reserved shortcuts cannot be used." })
    .waitFor();
  assert.equal(await shortcut.inputValue(), defaultShortcut);
  await page.keyboard.press("Tab");
  assert.notEqual(
    await shortcut.evaluate((element) => element === document.activeElement),
    true,
    "Tab leaves shortcut capture instead of being swallowed",
  );
  await shortcut.focus();
  await page.keyboard.press("Escape");
  await settings.waitFor({ state: "hidden" });
  assert.equal(await shortcut.inputValue(), defaultShortcut);
  const shortcutSettings = await openStudioSettings();
  const configurableShortcut = shortcutSettings.getByLabel(
    "Toggle sidebar shortcut",
    { exact: true },
  );
  await configurableShortcut.focus();
  await page.keyboard.press("Control+Shift+x");
  await page.waitForFunction(
    (element) => element.value === "Ctrl+Shift+X",
    await configurableShortcut.elementHandle(),
  );
  await page.keyboard.press("Escape");
  await shortcutSettings.waitFor({ state: "hidden" });

  const draft = "Keep this draft while Studio preferences change";
  const composer = page.locator("#message");
  await composer.fill(draft);
  const expandedBeforeTypingShortcut = await page
    .locator("#sidebar-toggle")
    .getAttribute("aria-expanded");
  await page.keyboard.press("Control+Shift+x");
  assert.equal(
    await page.locator("#sidebar-toggle").getAttribute("aria-expanded"),
    expandedBeforeTypingShortcut,
    "Typing in the composer does not trigger the sidebar shortcut",
  );
  assert.equal(await composer.inputValue(), draft);

  const setWidthAndCheckAlignment = async (edge) => {
    const dialog = await openStudioSettings();
    const slider = dialog.getByRole("slider", { name: "Transcript width" });
    await slider.press(edge);
    const expected = edge === "Home" ? "60" : "100";
    assert.equal(await slider.getAttribute("aria-valuenow"), expected);
    await page.keyboard.press("Escape");
    await dialog.waitFor({ state: "hidden" });
    const bounds = await page.evaluate(() => {
      const rect = (selector) => {
        const element = document.querySelector(selector);
        if (!element) return null;
        const { x, width } = element.getBoundingClientRect();
        return { x, width };
      };
      return {
        transcript: rect("#messages .message-content"),
        progress: rect(
          '.agent-panel[aria-label="Agent progress"][data-fit="yes"]',
        ),
        composer: rect("#conversation #composer"),
      };
    });
    assert.ok(Object.values(bounds).every(Boolean), JSON.stringify(bounds));
    for (const item of Object.values(bounds)) {
      assert.ok(
        Math.abs(item.x - bounds.transcript.x) <= 1,
        JSON.stringify(bounds),
      );
      assert.ok(
        Math.abs(item.width - bounds.transcript.width) <= 1,
        JSON.stringify(bounds),
      );
    }
    return bounds;
  };
  const narrowBounds = await setWidthAndCheckAlignment("Home");
  const wideBounds = await setWidthAndCheckAlignment("End");
  assert.ok(
    wideBounds.transcript.width > narrowBounds.transcript.width,
    "The width setting changes the shared transcript content width",
  );
  assert.equal(await composer.inputValue(), draft);

  await page.setViewportSize({ width: 390, height: 844 });
  const mobileSettings = await openStudioSettings();
  await page.keyboard.press("Escape");
  await mobileSettings.waitFor({ state: "hidden" });
  const mobileBounds = await page.evaluate(() => {
    const rect = (selector) => {
      const element = document.querySelector(selector);
      if (!element) return null;
      const { x, width } = element.getBoundingClientRect();
      return { x, width };
    };
    return {
      available: document.querySelector("#conversation").clientWidth,
      transcript: rect("#messages .message-content"),
      progress: rect(
        '.agent-panel[aria-label="Agent progress"][data-fit="yes"]',
      ),
      composer: rect("#conversation #composer"),
    };
  });
  for (const item of [
    mobileBounds.transcript,
    mobileBounds.progress,
    mobileBounds.composer,
  ]) {
    assert.ok(item, JSON.stringify(mobileBounds));
    assert.ok(item.x >= 0 && item.x + item.width <= 390 + 1);
  }
  for (const item of [mobileBounds.progress, mobileBounds.composer]) {
    assert.ok(Math.abs(item.x - mobileBounds.transcript.x) <= 1);
    assert.ok(Math.abs(item.width - mobileBounds.transcript.width) <= 1);
  }
  assert.ok(mobileBounds.transcript.width >= mobileBounds.available - 60);
  assert.equal(await composer.inputValue(), draft);

  await page.setViewportSize({ width: 1440, height: 960 });
  const reopen = await openStudioSettings();
  await page.keyboard.press("Escape");
  await reopen.waitFor({ state: "hidden" });
  await page.reload();
  await page.locator("#message").waitFor();
  await page.locator(".chat-row").filter({ hasText: "Release lead" }).click();
  await page
    .locator(
      '.agent-panel[aria-label="Agent progress"][data-fit="yes"] .agent-panel-current.progress-markdown',
    )
    .waitFor();
  assert.equal(
    await page.evaluate(() =>
      getComputedStyle(document.documentElement).getPropertyValue(
        "--studio-sidebar-font-size",
      ),
    ),
    "12px",
  );
  assert.equal(
    await page.evaluate(() =>
      getComputedStyle(document.documentElement).getPropertyValue(
        "--studio-main-font-size",
      ),
    ),
    "24px",
  );
  assert.equal(await page.locator("#message").inputValue(), draft);
  const sidebarExpanded = await page
    .locator("#sidebar-toggle")
    .getAttribute("aria-expanded");
  if (sidebarExpanded === "false")
    await page.locator("#sidebar-toggle").click();
  await page.locator(".chat-row").filter({ hasText: "Other project" }).click();
  await page.waitForFunction(
    () =>
      document.querySelector("#conversation-title").textContent ===
      "Other project",
  );
  assert.equal(
    await page.evaluate(() =>
      getComputedStyle(document.documentElement).getPropertyValue(
        "--studio-main-font-size",
      ),
    ),
    "24px",
    "Browser-wide settings remain in effect when another chat is selected",
  );

  const emptyPage = await browser.newPage({
    viewport: { width: 390, height: 844 },
  });
  emptyPage.on("pageerror", (error) => errors.push(error.message));
  const emptyState = {
    ...snapshot,
    stateDir: `${snapshot.stateDir}/empty-chat-settings`,
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
  await emptyPage.route("**/api/sync/identity", (route) =>
    route.fulfill({ status: 404, json: { error: "Fixture without sync" } }),
  );
  await emptyPage.route(/\/api\/state(?:\?.*)?$/, (route) =>
    route.fulfill({ json: emptyState }),
  );
  await emptyPage.goto(origin);
  await emptyPage
    .getByRole("button", { name: "Studio settings", exact: true })
    .waitFor();
  assert.equal(await emptyPage.locator("#message").count(), 0);
  const noChatSettings = await emptyPage
    .getByRole("button", { name: "Studio settings", exact: true })
    .click()
    .then(() =>
      emptyPage.getByRole("dialog", {
        name: "Studio settings",
        exact: true,
      }),
    );
  await noChatSettings.getByLabel("Studio theme", { exact: true }).waitFor();
  assert.deepEqual(errors, []);
  console.log(
    `PASS browser-local settings, invalid storage recovery, font and width bounds, shared layout, shortcut conflicts and typing scope, reload, chat scope, and no-chat settings. ${evidence}`,
  );
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
