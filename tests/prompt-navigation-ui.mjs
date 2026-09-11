#!/usr/bin/env node
// Production client with an isolated runtime. No model service or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(skill, "web/package.json"))(
  "playwright-core",
);
const root = await mkdtemp(join(tmpdir(), "codex-prompt-navigation-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  log = "";
proc.stderr.on("data", (d) => (log += d));
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const url = `http://127.0.0.1:${port}`;
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 1440, height: 960 },
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.setDefaultTimeout(12000);
  const state = await (await fetch(url + "/api/state")).json();
  const lead = state.runtime.agents.find((a) => a.name === "Release lead");
  const original = await (
    await fetch(url + "/api/transcript?id=" + lead.id)
  ).json();
  const prompts = Array.from({ length: 8 }, (_, i) => ({
    id: "prompt-" + i,
    role: "user",
    text: `Question ${i + 1}: Review component ${i + 1}`,
    title: "You",
  }));
  const items = prompts.flatMap((prompt, i) => [
    prompt,
    {
      id: "answer-" + i,
      role: "assistant",
      title: "Lead",
      text: Array.from(
        { length: 8 },
        (_, j) =>
          `Result ${i + 1}.${j + 1}. The component passed the scoped check. Review the evidence before release.`,
      ).join("\n\n"),
    },
  ]);
  const transcript = {
    ...original,
    items,
    order: items.map((m) => m.id),
    replace: true,
  };
  await page.route("**/api/transcript**", async (route) => {
    if (new URL(route.request().url()).searchParams.get("id") !== lead.id)
      return route.continue();
    if (new URL(route.request().url()).pathname === "/api/transcript/search") {
      const query = new URL(route.request().url()).searchParams
        .get("q")
        .toLowerCase();
      return route.fulfill({
        json: {
          results: items.filter((item) =>
            item.text.toLowerCase().includes(query),
          ),
        },
      });
    }
    if (route.request().url().includes("/stream?"))
      return route.fulfill({
        contentType: "text/event-stream",
        body: "data: " + JSON.stringify(transcript) + "\n\n",
      });
    await route.fulfill({ json: transcript });
  });
  // This suite verifies the native transcript transport without optional sync.
  await page.route("**/api/sync/identity", (r) =>
    r.fulfill({ status: 404, json: { error: "Unsupported sync" } }),
  );
  await page.goto(url);
  const openLead = async () => {
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Release lead" })
      .click();
    await page.locator('[data-message="prompt-0"]').waitFor();
  };
  const nav = page.getByRole("navigation", { name: "Conversation prompts" });
  await openLead();
  for (let i = 0; i < 30; i++) {
    await page.locator("#messages").evaluate((el, i) => {
      el.scrollTop = i % 2 ? el.scrollHeight : 0;
    }, i);
    await page.waitForTimeout(20);
    assert.equal(await nav.count(), 1, "scroll retains one prompt bar");
  }
  await page.locator("#messages").evaluate((el) => {
    el.scrollTop = 0;
  });
  await page.waitForFunction(
    () =>
      document.querySelector(".prompt-history-toggle")?.textContent.trim() ===
      "1 / 8",
  );
  assert.equal(
    await page
      .getByRole("button", { name: /^(Next|Previous) prompt$/ })
      .count(),
    0,
  );
  const composer = page.locator("#message");
  const scrollTop = await page
    .locator("#messages")
    .evaluate((el) => el.scrollTop);
  await composer.press("ArrowUp");
  assert.equal(
    await composer.inputValue(),
    prompts[7].text,
    "Up recalls the latest message",
  );
  await composer.press("ArrowUp");
  assert.equal(await composer.inputValue(), prompts[6].text);
  await composer.press("ArrowDown");
  assert.equal(await composer.inputValue(), prompts[7].text);
  await composer.press("ArrowDown");
  assert.equal(
    await composer.inputValue(),
    "",
    "Down restores the empty draft",
  );
  assert.equal(
    await page.locator("#messages").evaluate((el) => el.scrollTop),
    scrollTop,
    "Recall does not navigate the transcript",
  );
  for (let i = 0; i < 10; i++) await composer.press("ArrowUp");
  assert.equal(
    await composer.inputValue(),
    prompts[0].text,
    "Oldest entry clamps",
  );
  await composer.fill("Draft stays editable\nSecond line");
  await composer.press("ArrowUp");
  assert.equal(
    await composer.inputValue(),
    "Draft stays editable\nSecond line",
  );
  await composer.fill("");
  await composer.press("Shift+ArrowUp");
  assert.equal(
    await composer.inputValue(),
    "",
    "Modified arrows keep native behavior",
  );
  await composer.press("ArrowUp");
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await composer.fill("");
  await composer.press("ArrowUp");
  assert.equal(
    await composer.inputValue(),
    "",
    "Another chat cannot recall this chat's messages",
  );
  await openLead();
  await composer.fill("");
  await composer.press("ArrowDown");
  assert.equal(
    await composer.inputValue(),
    "",
    "Chat changes reset the history cursor",
  );
  await page
    .getByRole("button", { name: "Browse prompts", exact: true })
    .click();
  const history = page.getByRole("dialog", { name: "Prompt history" });
  await history
    .getByRole("button", { name: "Bookmark prompt 2", exact: true })
    .click();
  await history
    .getByRole("button", { name: "Show bookmarked prompts", exact: true })
    .click();
  assert.equal(await history.locator(".prompt-history-entry").count(), 1);
  await history.locator(".prompt-history-entry").click();
  for (let i = 0; i < 4; i++) {
    await page
      .locator("[data-chat]")
      .filter({ hasText: "Other project" })
      .click();
    await page.waitForFunction(
      () => !document.querySelector(".prompt-navigation"),
    );
    await openLead();
    assert.equal(
      await nav.count(),
      1,
      "chat change removes the previous navigation",
    );
  }
  await page.reload();
  await openLead();
  await page
    .getByRole("button", { name: "Browse prompts", exact: true })
    .click();
  assert.equal(
    await history
      .getByRole("button", {
        name: "Remove bookmark from prompt 2",
        exact: true,
      })
      .getAttribute("aria-pressed"),
    "true",
  );
  await history
    .getByRole("textbox", { name: "Search this chat" })
    .fill("component 6");
  await history
    .locator(".prompt-history-entry")
    .filter({ hasText: "Question 6" })
    .waitFor();
  assert.equal(await history.locator(".prompt-history-entry").count(), 1);
  await history.locator(".prompt-history-entry").click();
  await page.waitForFunction(
    () =>
      document.querySelector(".prompt-history-toggle")?.textContent.trim() ===
      "6 / 8",
  );
  await history.waitFor({ state: "hidden" });
  await page.screenshot({ path: join(root, "prompt-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await page
    .getByRole("button", { name: "Browse prompts", exact: true })
    .click();
  await page.waitForTimeout(250);
  const box = await history.boundingBox();
  assert.ok(
    box.x >= 0 && box.x + box.width <= 391,
    "prompt menu fits a narrow viewport",
  );
  assert.equal(await nav.count(), 1);
  await page.screenshot({ path: join(root, "prompt-mobile.png") });
  assert.deepEqual(errors, []);
  console.log(
    "PASS prompt navigation, bookmarks, scroll, chat changes, and responsive layout. Evidence: " +
      root,
  );
} finally {
  await browser?.close();
  proc.kill();
}
