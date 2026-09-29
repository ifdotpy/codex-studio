#!/usr/bin/env node
// Production composer against an isolated runtime and delayed skill endpoint.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-skill-autocomplete-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser;
let log = "";
fixture.stderr.on("data", (data) => (log += data));

const deferred = () => {
  let resolve;
  const promise = new Promise((done) => (resolve = done));
  return { promise, resolve: (value) => resolve(value) };
};

try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(`${origin}/api/state`)).json();
  const lead = state.threads.find((agent) => agent.name === "Release lead");
  const other = state.threads.find((agent) => agent.name === "Other project");
  assert.ok(lead && other);

  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  const page = await browser.newPage({
    viewport: { width: 1280, height: 900 },
  });
  await page.addInitScript(() => {
    window.__skillTestNow = Date.now();
    Date.now = () => window.__skillTestNow;
  });
  page.setDefaultTimeout(10000);
  const pageErrors = [];
  const skillRequests = [];
  const sends = [];
  const queues = [];
  let typingFrameMs = null;
  let catalogSkillCount = 0;
  let sendHandled = deferred();
  page.on("pageerror", (error) => pageErrors.push(error.message));

  const gates = new Map();
  const stubSessionCosts = (targetPage) =>
    targetPage.route("**/api/session-cost?*", (route) =>
      route.fulfill({ json: { totalUSD: 0, breakdown: { providers: {} } } }),
    );
  await stubSessionCosts(page);
  await page.route("**/api/skills?*", async (route) => {
    const agent = new URL(route.request().url()).searchParams.get("agent");
    skillRequests.push(agent);
    let gate = gates.get(agent) || gates.get(lead.id);
    if (!gate) gates.set(agent, (gate = deferred()));
    else if (!gates.has(agent)) gates.set(agent, gate);
    const catalog = await gate.promise;
    await route.fulfill({ json: catalog });
  });
  await page.route("**/api/messages", async (route) => {
    const message = route.request().postDataJSON();
    sends.push(message);
    sendHandled.resolve();
    await route.fulfill({ json: { id: message.id, status: "accepted" } });
  });
  await page.route("**/api/queue", async (route) => {
    if (route.request().method() === "POST") {
      queues.push(route.request().postDataJSON());
      await route.fulfill({ json: { items: [] } });
    } else {
      await route.fulfill({ json: { items: [] } });
    }
  });

  await page.goto(origin);
  await page.locator(`[data-chat="${lead.id}"]`).click();
  await page.waitForFunction(() =>
    document
      .querySelector("#conversation-title")
      ?.textContent.includes("Release lead"),
  );
  const composer = page.locator("#message");
  await composer.waitFor();

  // Existing prompt recall still works outside an active suggestion.
  await composer.press("ArrowUp");
  await page.waitForFunction(() =>
    document.querySelector("#message")?.value.includes("Review the release"),
  );
  await composer.fill("");

  // A slow catalog leaves the textarea live and one request serves all typing.
  await composer.type("$r");
  await page
    .locator("#skill-suggestions")
    .waitFor()
    .catch(async (error) => {
      console.error(
        "autocomplete debug",
        await page.evaluate(() => ({
          value: document.querySelector("#message")?.value,
          aria: document
            .querySelector("#message")
            ?.getAttribute("aria-expanded"),
          suggestions: document.querySelector("#skill-suggestions")?.outerHTML,
        })),
      );
      throw error;
    });
  await composer.type("e");
  assert.equal(await composer.inputValue(), "$re");
  assert.deepEqual(skillRequests, [lead.id]);
  await composer.press("Enter");
  await composer.press("Tab");
  assert.equal(
    await composer.inputValue(),
    "$re",
    "loading keys do not edit the prompt",
  );
  assert.equal(sends.length, 0, "loading Enter does not send");
  assert.equal(queues.length, 0, "loading Tab does not queue");
  assert.equal(
    await composer.evaluate((element) => document.activeElement === element),
    true,
    "loading Tab keeps focus in the composer",
  );

  // A late result stays hidden after changing chats until a new token is typed.
  await composer.fill("");
  await page.locator(`[data-chat="${other.id}"]`).click();
  await page.waitForFunction(() =>
    document
      .querySelector("#conversation-title")
      ?.textContent.includes("Other project"),
  );
  assert.equal(await page.locator("#skill-suggestions").count(), 0);
  const catalogSkills = [
    {
      name: "review",
      description: "Review a change",
      path: "/skills/review",
    },
    {
      name: "release",
      description: "Prepare a release",
      path: "/skills/release",
    },
    {
      name: "research",
      description: "Research a topic",
      path: "/skills/research",
    },
    {
      name: "plugin-management:plugin-management",
      description: "Installed namespaced skill",
      path: "/skills/plugin-management/plugin-management",
    },
    {
      name: "openai-templates:artifact-template-analytics-dashboard",
      description: "A long namespaced native skill for narrow composer layouts",
      path: "/skills/openai-templates/artifact-template-analytics-dashboard",
    },
    ...Array.from({ length: 500 }, (_, index) => ({
      name: `review-extra-${index}`,
      description: "Large-catalog fixture",
      path: `/skills/review-extra-${index}`,
    })),
  ];
  catalogSkillCount = catalogSkills.length;
  gates.get(lead.id).resolve({
    skills: catalogSkills,
    errors: [],
  });
  await page.waitForTimeout(50);
  assert.equal(await page.locator("#skill-suggestions").count(), 0);
  await composer.fill("$re");
  const review = page.getByRole("option", { name: /review Review a change/ });
  await review.waitFor();
  assert.equal(
    await page.getByRole("option").count(),
    8,
    "local filtering caps the visible list with a large catalog",
  );
  assert.equal(
    catalogSkillCount,
    505,
    "fixture has 500+ local catalog entries",
  );
  const timingSession = await page.context().newCDPSession(page);
  await timingSession.send("Emulation.setCPUThrottlingRate", { rate: 4 });
  await composer.evaluate((element) => {
    element.addEventListener(
      "keydown",
      (event) => {
        if (event.key !== "v") return;
        const started = performance.now();
        requestAnimationFrame(() => {
          requestAnimationFrame(() => {
            window.__skillTypingFrameMs = performance.now() - started;
          });
        });
      },
      { once: true },
    );
  });
  await composer.press("v");
  await page.waitForFunction(() => window.__skillTypingFrameMs != null);
  typingFrameMs = await page.evaluate(() => window.__skillTypingFrameMs);
  await timingSession.send("Emulation.setCPUThrottlingRate", { rate: 1 });
  await timingSession.detach();
  assert.ok(
    typingFrameMs < 250,
    `500-entry local filter next-frame latency ${typingFrameMs.toFixed(1)} ms at 4x CPU`,
  );
  assert.equal(await composer.inputValue(), "$rev");
  await composer.press("Tab");
  assert.equal(await composer.inputValue(), "$review ");
  assert.equal(sends.length, 0, "Tab selection does not send");
  assert.equal(queues.length, 0, "Tab selection does not queue");

  await composer.fill("");
  await composer.fill("$plugin-management:plugin");
  await page
    .getByRole("option", { name: /plugin-management:plugin-management/ })
    .waitFor();
  await composer.press("Enter");
  assert.equal(
    await composer.inputValue(),
    "$plugin-management:plugin-management ",
    "namespaced native skill names stay intact",
  );

  await composer.fill("");
  await composer.fill("before $plugin-management:plugin-wrong suffix");
  await composer.evaluate((element) => {
    const caret = element.value.indexOf("-wrong") + 1;
    element.focus();
    element.setSelectionRange(caret, caret);
    element.dispatchEvent(new Event("select", { bubbles: true }));
  });
  await page
    .getByRole("option", { name: /plugin-management:plugin-management/ })
    .waitFor();
  await composer.press("Enter");
  assert.equal(
    await composer.inputValue(),
    "before $plugin-management:plugin-management suffix",
    "namespaced selection replaces the full token without leaving a suffix",
  );

  await composer.fill("");
  await composer.fill("$rev");
  await page
    .getByRole("combobox", { name: "Message", expanded: true })
    .waitFor();
  assert.equal(await composer.getAttribute("aria-autocomplete"), "list");
  assert.equal(
    await composer.getAttribute("aria-controls"),
    "skill-suggestions",
  );
  const reviewMatches = page.getByRole("option");
  await reviewMatches.first().waitFor();
  assert.equal(
    await reviewMatches.first().getAttribute("aria-selected"),
    "true",
    "the first suggestion is selected by default",
  );
  await composer.press("ArrowDown");
  assert.equal(
    await reviewMatches.nth(1).getAttribute("aria-selected"),
    "true",
  );
  await composer.press("ArrowUp");
  assert.equal(
    await reviewMatches.first().getAttribute("aria-selected"),
    "true",
  );
  // A constrained popup must scroll selection without moving textarea focus.
  await page.locator("#skill-suggestions").evaluate((list) => {
    list.style.maxHeight = "100px";
  });
  const optionCount = await reviewMatches.count();
  for (let index = 1; index < optionCount; index++) {
    await composer.press("ArrowDown");
  }
  const assertVisibleSelection = async () => {
    const state = await page.locator("#skill-suggestions").evaluate((list) => {
      const selected = list.querySelector('[aria-selected="true"]');
      const row = selected.getBoundingClientRect();
      const box = list.getBoundingClientRect();
      return {
        visible: row.top >= box.top - 1 && row.bottom <= box.bottom + 1,
        scrollTop: list.scrollTop,
        focused: document.activeElement?.id === "message",
        activeDescendant:
          document.activeElement?.getAttribute("aria-activedescendant") ===
          selected.id,
      };
    });
    assert.equal(
      state.visible,
      true,
      "keyboard selection stays inside the scroll viewport",
    );
    assert.equal(
      state.focused,
      true,
      "arrow navigation retains textarea focus",
    );
    assert.equal(
      state.activeDescendant,
      true,
      "ARIA tracks the visible selection",
    );
    return state;
  };
  assert.equal(
    await reviewMatches.last().getAttribute("aria-selected"),
    "true",
  );
  assert.ok((await assertVisibleSelection()).scrollTop > 0);
  await composer.press("ArrowDown");
  assert.equal(
    await reviewMatches.first().getAttribute("aria-selected"),
    "true",
  );
  assert.equal((await assertVisibleSelection()).scrollTop, 0);
  await composer.press("ArrowUp");
  assert.equal(
    await reviewMatches.last().getAttribute("aria-selected"),
    "true",
  );
  await assertVisibleSelection();
  await composer.press("ArrowDown");
  assert.equal(await composer.inputValue(), "$rev");
  await composer.press("Enter");
  assert.equal(await composer.inputValue(), "$review ");

  // Click selection keeps the textarea focused and replaces only the token.
  await composer.fill("before $rel after");
  await composer.evaluate((element) => {
    const caret = element.value.indexOf(" after");
    element.focus();
    element.setSelectionRange(caret, caret);
    element.dispatchEvent(new Event("select", { bubbles: true }));
  });
  const release = page.getByRole("option", {
    name: /release Prepare a release/,
  });
  await release.waitFor();
  await release.click();
  assert.equal(await composer.inputValue(), "before $release after");
  await page.waitForFunction(
    () =>
      document.querySelector("#message")?.selectionStart ===
      "before $release".length,
  );
  assert.equal(
    await composer.evaluate((element) => document.activeElement === element),
    true,
  );
  assert.equal(
    await composer.evaluate((element) => element.selectionStart),
    "before $release".length,
    "caret remains at the insertion point",
  );

  await composer.fill("$rel");
  await page
    .getByRole("option", { name: /release Prepare a release/ })
    .waitFor();
  await composer.press("Enter");
  assert.equal(await composer.inputValue(), "$release ");
  assert.equal(sends.length, 0, "Enter selection does not send");

  // Escape closes; an unselected Enter still follows the normal send path.
  await composer.fill("$rev");
  await page.getByRole("option").first().waitFor();
  await composer.evaluate((element) => element.blur());
  await page.locator("#skill-suggestions").waitFor({ state: "detached" });
  await composer.click();
  await page.getByRole("option").first().waitFor();
  await composer.press("Escape");
  await page.locator("#skill-suggestions").waitFor({ state: "detached" });
  await composer.fill("ordinary message");
  sendHandled = deferred();
  await composer.press("Enter");
  await sendHandled.promise;
  await page.waitForFunction(
    () => document.querySelector("#message")?.value === "",
  );

  // An unselected Tab preserves the existing queue shortcut.
  await composer.fill("queue this");
  sendHandled = deferred();
  await composer.press("Tab");
  await sendHandled.promise;
  await page.waitForFunction(
    () => document.querySelector("#message")?.value === "",
  );
  assert.equal(sends.length, 2);
  assert.equal(
    sends[1].delivery,
    "after_turn",
    `unselected Tab keeps queue behavior: ${JSON.stringify(sends)}`,
  );

  // IME Enter does not select or submit; dollar amounts do not open a list.
  await composer.fill("$rev");
  await page.getByRole("option").first().waitFor();
  await composer.evaluate((element) =>
    element.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Enter",
        bubbles: true,
        isComposing: true,
        keyCode: 229,
      }),
    ),
  );
  assert.equal(await composer.inputValue(), "$rev");
  await composer.press("Escape");
  await composer.fill("$123.45");
  await page.locator("#skill-suggestions").waitFor({ state: "detached" });
  assert.deepEqual(skillRequests, [lead.id], "no per-key catalog reads");

  // The selected chat reuses a valid catalog from its shared scope cache.
  await composer.fill("$r");
  await page.getByRole("option", { name: /review Review a change/ }).waitFor();
  assert.deepEqual(
    skillRequests,
    [lead.id],
    "same-scope cache avoids another request",
  );
  await page.evaluate(() => {
    window.__skillTestNow += 3 * 60 * 1000;
  });
  await composer.press("Escape");
  await composer.fill("");
  const refreshedCatalog = deferred();
  gates.set(other.id, refreshedCatalog);
  await composer.fill("$r");
  await page.getByText("Loading skills…", { exact: true }).waitFor();
  await composer.press("Tab");
  assert.equal(
    await composer.inputValue(),
    "$r",
    "Tab does not select stale cached matches during refresh",
  );
  assert.equal(queues.length, 0, "loading Tab cannot queue stale matches");
  refreshedCatalog.resolve({ skills: catalogSkills, errors: [] });
  await page.getByRole("option", { name: /review Review a change/ }).waitFor();
  assert.deepEqual(
    skillRequests,
    [lead.id, other.id],
    "reopening after expiry refreshes the scoped catalog",
  );

  // Preserve modified Enter and Shift+Enter as ordinary textarea behavior.
  await composer.fill("line one");
  await composer.press("Shift+Enter");
  await composer.type("line two");
  assert.equal(await composer.inputValue(), "line one\nline two");
  sendHandled = deferred();
  await composer.press("Control+Enter");
  await sendHandled.promise;
  assert.equal(sends.length, 3);
  assert.equal(
    sends[2].text,
    "line one\nline two",
    "modified Enter keeps normal send behavior",
  );
  assert.equal(queues.length, 0);
  assert.deepEqual(pageErrors, []);

  const failurePage = await browser.newPage({
    viewport: { width: 1280, height: 900 },
  });
  await stubSessionCosts(failurePage);
  failurePage.setDefaultTimeout(10000);
  let failureReads = 0;
  const failureSends = [];
  let failureSendHandled = deferred();
  await failurePage.route("**/api/skills?*", async (route) => {
    failureReads++;
    await route.fulfill({ status: 503, json: { error: "Skills are offline" } });
  });
  await failurePage.route("**/api/messages", async (route) => {
    const message = route.request().postDataJSON();
    failureSends.push(message);
    failureSendHandled.resolve(message);
    await route.fulfill({ json: { id: message.id, status: "accepted" } });
  });
  await failurePage.route("**/api/queue", async (route) => {
    await route.fulfill({ json: { items: [] } });
  });
  await failurePage.goto(origin);
  await failurePage.locator(`[data-chat="${lead.id}"]`).click();
  await failurePage.waitForFunction(() =>
    document
      .querySelector("#conversation-title")
      ?.textContent.includes("Release lead"),
  );
  const failedComposer = failurePage.locator("#message");
  await failedComposer.fill("$");
  await failurePage
    .getByText("Could not load skills", { exact: true })
    .waitFor();
  await failedComposer.press("Enter");
  await Promise.race([
    failureSendHandled.promise,
    failurePage.waitForTimeout(5000).then(() => {
      throw new Error("Timed out waiting for failed-list Enter send");
    }),
  ]);
  assert.equal(
    failureSends[0]?.text,
    "$",
    "failed-list Enter preserves normal send",
  );
  await failedComposer.fill("$keep-typing");
  await failurePage
    .getByText("Could not load skills", { exact: true })
    .waitFor();
  failureSendHandled = deferred();
  await failedComposer.press("Tab");
  await Promise.race([
    failureSendHandled.promise,
    failurePage.waitForTimeout(5000).then(() => {
      throw new Error("Timed out waiting for failed-list Tab queue");
    }),
  ]);
  assert.equal(
    failureSends[1]?.text,
    "$keep-typing",
    "failed-list Tab preserves normal queue behavior",
  );
  assert.equal(failureSends[1]?.delivery, "after_turn");
  assert.equal(failureReads, 1, "failed catalogs are not refetched per key");
  assert.equal(
    await failurePage.evaluate(
      () => document.documentElement.scrollWidth > innerWidth,
    ),
    false,
    "mobile composer feedback does not cause horizontal overflow",
  );

  const mobilePage = await browser.newPage({
    viewport: { width: 390, height: 844 },
  });
  await stubSessionCosts(mobilePage);
  mobilePage.setDefaultTimeout(10000);
  await mobilePage.route("**/api/skills?*", (route) =>
    route.fulfill({
      json: {
        skills: [
          {
            name: "openai-templates:artifact-template-analytics-dashboard",
            description: "Long native namespace name",
            path: "/skills/openai-templates/artifact-template-analytics-dashboard",
          },
        ],
        errors: [],
      },
    }),
  );
  await mobilePage.goto(origin);
  await mobilePage
    .getByRole("button", { name: "Toggle conversations" })
    .click();
  await mobilePage.locator(`[data-chat="${lead.id}"]`).click();
  await mobilePage.waitForFunction(() =>
    document
      .querySelector("#conversation-title")
      ?.textContent.includes("Release lead"),
  );
  const mobileComposer = mobilePage.locator("#message");
  await mobileComposer.fill("$openai-templates:artifact-template");
  await mobilePage
    .getByRole("option", {
      name: /openai-templates:artifact-template-analytics-dashboard/,
    })
    .waitFor();
  assert.equal(
    await mobilePage.evaluate(
      () => document.documentElement.scrollWidth > innerWidth,
    ),
    false,
    "long namespaced skills fit a 390px viewport",
  );
  assert.equal(
    await mobilePage
      .locator("#skill-suggestions")
      .evaluate((list) => list.scrollWidth > list.clientWidth),
    false,
    "long namespaced skill names do not overflow the popup",
  );

  const emptyPage = await browser.newPage({
    viewport: { width: 1280, height: 900 },
  });
  await stubSessionCosts(emptyPage);
  emptyPage.setDefaultTimeout(10000);
  const emptySends = [];
  let emptySendHandled = deferred();
  await emptyPage.route("**/api/skills?*", (route) =>
    route.fulfill({
      json: { skills: [], errors: ["One skills folder was unavailable"] },
    }),
  );
  await emptyPage.route("**/api/messages", async (route) => {
    const message = route.request().postDataJSON();
    emptySends.push(message);
    emptySendHandled.resolve(message);
    await route.fulfill({ json: { id: message.id, status: "accepted" } });
  });
  await emptyPage.route("**/api/queue", async (route) => {
    await route.fulfill({ json: { items: [] } });
  });
  await emptyPage.goto(origin);
  await emptyPage.locator(`[data-chat="${lead.id}"]`).click();
  await emptyPage.waitForFunction(() =>
    document
      .querySelector("#conversation-title")
      ?.textContent.includes("Release lead"),
  );
  const emptyComposer = emptyPage.locator("#message");
  await emptyComposer.fill("$missing");
  await emptyPage.getByText("No matching skills", { exact: true }).waitFor();
  await emptyPage
    .getByText("Some skills could not be loaded", { exact: true })
    .waitFor();
  assert.equal(await emptyComposer.inputValue(), "$missing");
  await emptyComposer.press("Enter");
  await Promise.race([
    emptySendHandled.promise,
    emptyPage.waitForTimeout(5000).then(() => {
      throw new Error("Timed out waiting for no-match Enter send");
    }),
  ]);
  assert.equal(
    emptySends[0]?.text,
    "$missing",
    "no-match Enter still sends normally",
  );
  await emptyComposer.fill("$queue-missing");
  await emptyPage.getByText("No matching skills", { exact: true }).waitFor();
  emptySendHandled = deferred();
  await emptyComposer.press("Tab");
  await Promise.race([
    emptySendHandled.promise,
    emptyPage.waitForTimeout(5000).then(() => {
      throw new Error("Timed out waiting for no-match Tab queue");
    }),
  ]);
  assert.equal(
    emptySends[1]?.text,
    "$queue-missing",
    "no-match Tab still queues normally",
  );
  assert.equal(emptySends[1]?.delivery, "after_turn");
  await failurePage.close();
  await emptyPage.close();
  await mobilePage.close();
  console.log(
    JSON.stringify({
      ok: true,
      evidence,
      timing: {
        cpuThrottle: "4x",
        catalogSkills: catalogSkillCount,
        keydownToPostPaintFrameMs: Number(typingFrameMs.toFixed(1)),
      },
      checks: [
        "slow lazy fetch keeps typing responsive and deduplicates",
        "local filtering, default Enter/Tab insertion, caret and click focus",
        "Escape, send, queue, IME, amount suppression and stale chat results",
        "fresh catalogs are reused and refreshed after bounded expiry",
        "loading/error keys cannot choose hidden stale matches; blur closes popup",
        "failed and no-match lists retain normal send/queue behavior",
        "long namespaced skill fits 390px and measured local-filter responsiveness",
        "prompt recall and modified Enter behavior",
        "responsive error, empty, and partial-catalog feedback",
      ],
    }),
  );
} catch (error) {
  console.error(error);
  console.error(log);
  process.exitCode = 1;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
