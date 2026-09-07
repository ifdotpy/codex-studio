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
    if (route.request().url().includes("/stream?"))
      return route.fulfill({
        contentType: "text/event-stream",
        body: "data: " + JSON.stringify(transcript) + "\n\n",
      });
    await route.fulfill({ json: transcript });
  });
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
  await page.getByRole("button", { name: "Next prompt", exact: true }).click();
  await page.waitForFunction(
    () =>
      document.querySelector(".prompt-history-toggle")?.textContent.trim() ===
      "2 / 8",
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
    .getByRole("textbox", { name: "Find a prompt" })
    .fill("component 6");
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
