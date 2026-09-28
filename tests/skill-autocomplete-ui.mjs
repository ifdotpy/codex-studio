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
  page.setDefaultTimeout(10000);
  const pageErrors = [];
  const skillRequests = [];
  const sends = [];
  const queues = [];
  let sendHandled = deferred();
  page.on("pageerror", (error) => pageErrors.push(error.message));

  const gates = new Map();
  await page.route("**/api/skills?*", async (route) => {
    const agent = new URL(route.request().url()).searchParams.get("agent");
    skillRequests.push(agent);
    let gate = gates.get(agent);
    if (!gate) gates.set(agent, (gate = deferred()));
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

  // A late result stays hidden after changing chats until a new token is typed.
  await composer.fill("");
  await page.locator(`[data-chat="${other.id}"]`).click();
  await page.waitForFunction(() =>
    document
      .querySelector("#conversation-title")
      ?.textContent.includes("Other project"),
  );
  assert.equal(await page.locator("#skill-suggestions").count(), 0);
  gates.get(lead.id).resolve({
    skills: [
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
    ],
    errors: [],
  });
  await page.waitForTimeout(50);
  assert.equal(await page.locator("#skill-suggestions").count(), 0);
  await composer.fill("$re");
  const review = page.getByRole("option", { name: /review Review a change/ });
  await review.waitFor();
  assert.equal(
    await page.getByRole("option").count(),
    3,
    "local name filtering",
  );
  await composer.press("ArrowDown");
  await composer.press("Tab");
  assert.equal(await composer.inputValue(), "$review ");
  assert.equal(sends.length, 0, "Tab selection does not send");
  assert.equal(queues.length, 0, "Tab selection does not queue");

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
  await composer.press("ArrowDown");
  await composer.press("Enter");
  assert.equal(await composer.inputValue(), "$release ");
  assert.equal(sends.length, 0, "Enter selection does not send");

  // Escape closes; an unselected Enter still follows the normal send path.
  await composer.fill("$rev");
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
    "queue",
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
  await page.getByRole("option", { name: /review/ }).waitFor();
  assert.deepEqual(
    skillRequests,
    [lead.id],
    "same-scope cache avoids another request",
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
    viewport: { width: 390, height: 844 },
  });
  failurePage.setDefaultTimeout(10000);
  let failureReads = 0;
  await failurePage.route("**/api/skills?*", async (route) => {
    failureReads++;
    await route.fulfill({ status: 503, json: { error: "Skills are offline" } });
  });
  await failurePage.goto(origin);
  await failurePage
    .getByRole("button", { name: "Toggle conversations" })
    .click();
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
  await failedComposer.type("keep-typing");
  assert.equal(await failedComposer.inputValue(), "$keep-typing");
  assert.equal(failureReads, 1, "failed catalogs are not refetched per key");
  assert.equal(
    await failurePage.evaluate(
      () => document.documentElement.scrollWidth > innerWidth,
    ),
    false,
    "mobile composer feedback does not cause horizontal overflow",
  );

  const emptyPage = await browser.newPage({
    viewport: { width: 390, height: 844 },
  });
  emptyPage.setDefaultTimeout(10000);
  await emptyPage.route("**/api/skills?*", (route) =>
    route.fulfill({
      json: { skills: [], errors: ["One skills folder was unavailable"] },
    }),
  );
  await emptyPage.goto(origin);
  await emptyPage.getByRole("button", { name: "Toggle conversations" }).click();
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
  await failurePage.close();
  await emptyPage.close();
  console.log(
    JSON.stringify({
      ok: true,
      evidence,
      checks: [
        "slow lazy fetch keeps typing responsive and deduplicates",
        "local filtering, arrows, Tab insertion, caret and click focus",
        "Escape, send, queue, IME, amount suppression and stale chat results",
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
