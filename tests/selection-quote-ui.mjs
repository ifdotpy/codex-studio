#!/usr/bin/env node
// Exercise real message selection and draft changes against an isolated runtime.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(skill, "web/package.json"));
const { chromium } = require("playwright-core");
const root = await mkdtemp(join(tmpdir(), "codex-selection-quote-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
  },
);
let log = "",
  browser;
fixture.stderr.on("data", (data) => (log += data));
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
    args: ["--disable-extensions", "--no-first-run"],
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
    hasTouch: true,
  });
  page.setDefaultTimeout(12000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(origin);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  const prose = page.locator("article.assistant .prose").first();
  await prose.getByText("I assigned 40 workers", { exact: false }).waitFor();
  const paragraphs = prose.locator(".markdown-block p");
  const action = page.getByRole("button", {
    name: "Quote selection",
    exact: true,
  });
  const draft = page.locator("#message");
  const select = async (startText, endText = startText) => {
    await paragraphs.first().scrollIntoViewIfNeeded();
    return prose.evaluate(
      (element, { startText, endText }) => {
        const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT);
        let node, first, last;
        while ((node = walker.nextNode())) {
          if (!first && node.textContent.includes(startText)) first = node;
          if (node.textContent.includes(endText)) last = node;
        }
        if (!first || !last) throw Error("Fixture excerpt missing");
        const range = document.createRange();
        range.setStart(first, first.textContent.indexOf(startText));
        range.setEnd(last, last.textContent.indexOf(endText) + endText.length);
        const selection = window.getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
        return selection.toString();
      },
      { startText, endText },
    );
  };
  const append = (previous, excerpt) =>
    `${previous}${!previous || previous.endsWith("\n\n") ? "" : previous.endsWith("\n") ? "\n" : "\n\n"}${excerpt
      .split(/\r?\n/)
      .map((line) => `> ${line}`)
      .join("\n")}\n\n`;
  let expected = "My existing comment";
  await draft.fill(expected);
  await select("40 workers");
  await action.click();
  expected = append(expected, "40 workers");
  assert.equal(
    await draft.inputValue(),
    expected,
    "exact partial text appends without replacing a comment",
  );
  await select("Seven workers are active");
  await action.click();
  expected = append(expected, "Seven workers are active");
  assert.equal(
    await draft.inputValue(),
    expected,
    "second quote from the same paragraph preserves order",
  );
  await draft.fill(`${expected}My next question`);
  expected += "My next question";
  const crossing = await select(
    "15 have finished.",
    "Worker 07 found a failed check.",
  );
  assert.ok(crossing.includes("\n"), "browser retains paragraph boundary");
  await action.click();
  expected = append(expected, crossing);
  assert.equal(
    await draft.inputValue(),
    expected,
    "every paragraph line remains inside the quote",
  );
  await select("remaining results");
  await page.keyboard.press("Alt+Shift+KeyQ");
  expected = append(expected, "remaining results");
  assert.equal(
    await draft.inputValue(),
    expected,
    "keyboard shortcut quotes the selected slice",
  );
  await select("release report");
  await action.focus();
  await page.keyboard.press("Enter");
  expected = append(expected, "release report");
  assert.equal(
    await draft.inputValue(),
    expected,
    "focused quote action supports keyboard activation",
  );
  await select("40 workers");
  await prose
    .locator("..")
    .getByRole("button", { name: "Quote message", exact: true })
    .click();
  expected = append(expected, "40 workers");
  assert.equal(
    await draft.inputValue(),
    expected,
    "message quote action prefers its current selection",
  );

  // Selection across message boundaries must not include labels or controls.
  await page.locator("#messages").evaluate((element) => {
    const first = element.querySelector("article.user .prose").firstChild;
    const last = element.querySelector("article.assistant .prose p").firstChild;
    const range = document.createRange();
    range.setStart(first, 1);
    range.setEnd(last, 8);
    window.getSelection().removeAllRanges();
    window.getSelection().addRange(range);
  });
  await action.waitFor({ state: "hidden" });
  await page.keyboard.press("Alt+Shift+KeyQ");
  assert.equal(
    await draft.inputValue(),
    expected,
    "cross-message selection is rejected",
  );
  await draft.selectText();
  await action.waitFor({ state: "hidden" });
  assert.equal(
    await draft.inputValue(),
    expected,
    "composer selection is not a message quote",
  );

  await select("40 workers");
  await action.waitFor();
  await page.screenshot({ path: join(root, "selection-quote-desktop.png") });
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await action.waitFor({ state: "hidden" });
  await page.keyboard.press("Alt+Shift+KeyQ");
  assert.equal(
    await draft.inputValue(),
    "",
    "chat change cannot reuse an old excerpt",
  );
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  assert.equal(
    await draft.inputValue(),
    expected,
    "original chat retains all quotes and comments",
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await select("40 workers");
  await action.waitFor();
  const bounds = await action.boundingBox();
  assert.ok(
    bounds.x >= 0 && bounds.x + bounds.width <= 390,
    "selection action fits a narrow viewport",
  );
  await page.screenshot({ path: join(root, "selection-quote-mobile.png") });
  await action.tap();
  expected = append(expected, "40 workers");
  assert.equal(
    await draft.inputValue(),
    expected,
    "touch activation appends one quote",
  );
  assert.ok(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth + 1,
    ),
    "no horizontal overflow",
  );
  await page.setViewportSize({ width: 1440, height: 960 });
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await draft.fill("Preview selection fixture");
  await page.locator("#send").click();
  let previewAgent;
  for (let attempt = 0; attempt < 100; attempt++) {
    const state = await (await fetch(`${origin}/api/state`)).json();
    previewAgent = state.threads.find(
      (agent) => agent.name === "Other project",
    );
    if (previewAgent.status === "running" && previewAgent.turnId) break;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  assert.ok(previewAgent.turnId, "fixture turn starts");
  fixture.stdin.write(
    JSON.stringify({
      method: "item/completed",
      params: {
        threadId: previewAgent.threadId,
        turnId: previewAgent.turnId,
        item: {
          id: "selection-preview",
          type: "agentMessage",
          text: "Before the preview.\n\n```html\n<div>Static card</div>\n```\n\nAfter the preview.",
        },
      },
    }) + "\n",
  );
  const previewProse = page.locator("article.assistant .prose").first();
  await previewProse.locator(".rich-preview-toolbar").waitFor();
  await previewProse.evaluate((element) => {
    const range = document.createRange();
    range.setStart(element.querySelector(".markdown-block p").firstChild, 0);
    range.setEnd(
      element.querySelector(".markdown-block:last-child p").firstChild,
      5,
    );
    window.getSelection().removeAllRanges();
    window.getSelection().addRange(range);
  });
  await action.waitFor({ state: "hidden" });
  await page.keyboard.press("Alt+Shift+KeyQ");
  assert.equal(
    await draft.inputValue(),
    "",
    "a prose range that crosses preview controls cannot enter a quote",
  );
  assert.deepEqual(errors, []);
  console.log(
    "Selection quote UI: PASS (exact slices, repeated quotes, preserved comments, paragraphs, keyboard, fallback, scope, chat switch, touch, mobile)",
  );
  console.log(`Browser evidence: ${root}`);
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
