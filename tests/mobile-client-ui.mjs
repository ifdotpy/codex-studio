#!/usr/bin/env node
// Isolated server and headless browser. No model requests or user state.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, writeFile, realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const root = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(root, "web/package.json"))(
  "playwright-core",
);
const state = await mkdtemp(join(tmpdir(), "codex-mobile-ui-"));
const fixture = spawn(
  "python3",
  ["-B", join(root, "tests/simple-ui-fixture.py"), state],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let browser,
  log = "";
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
  const url = `http://127.0.0.1:${port}`;
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  const page = await browser.newPage({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
  });
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto(url);
  await page.locator("#message").waitFor();
  for (const width of [320, 390, 760]) {
    await page.setViewportSize({ width, height: 844 });
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .waitFor();
    assert.equal(await page.locator(".workspace-shortcuts").count(), 0);
    const snapshot = await (await fetch(url + "/api/state")).json();
    const selected = await page.evaluate(() =>
      JSON.parse(localStorage.getItem("codex-mobile-opened")),
    );
    const hasWorkers = snapshot.threads.some(
      (a) => a.rootId === selected && !a.isLead,
    );
    assert.equal(
      await page.locator("#team-toggle").count(),
      hasWorkers ? 1 : 0,
    );
    assert.equal(await page.locator(".terminal-dock").count(), 0);
    assert.equal(await page.locator(".usage-footer").count(), 0);
    assert.ok(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth,
      ),
      `Page overflows at ${width}`,
    );
    const composer = await page.locator("#composer").boundingBox();
    assert.ok(composer.x >= 0 && composer.x + composer.width <= width + 1);
    await page.getByLabel("Toggle conversations").click();
    await page.getByLabel("Search chats", { exact: true }).waitFor();
    assert.equal(
      await page.getByRole("tab", { name: /^Agent chats/ }).count(),
      0,
    );
    assert.equal(
      await page.getByLabel("Add project", { exact: true }).count(),
      0,
    );
    assert.equal(
      await page.getByRole("button", { name: "Complaint book" }).count(),
      0,
    );
    assert.ok(
      (await page.getByRole("button", { name: /^New chat in / }).count()) > 0,
    );
    await page.getByLabel("Close conversations", { exact: true }).click();
    await page.locator("#sidebar").waitFor({ state: "hidden" });
  }
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: join(state, "mobile-390.png") });
  console.log("Mobile screenshot:", join(state, "mobile-390.png"));
  await page.locator("#message").fill("First line");
  await page.locator("#message").press("Enter");
  assert.equal(await page.locator("#message").inputValue(), "First line\n");
  assert.ok(
    Number(
      await page
        .locator("#message")
        .evaluate((element) => parseFloat(getComputedStyle(element).fontSize)),
    ) >= 16,
  );
  await page
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  await page.getByLabel("Chat account", { exact: true }).waitFor();
  assert.equal(
    await page.getByRole("button", { name: /Add account/ }).count(),
    0,
  );
  assert.equal(
    await page.getByLabel("Subagent defaults", { exact: true }).count(),
    1,
  );
  let originalBody, created;
  let confirmServerCreation;
  const serverCreated = new Promise((resolve) => {
    confirmServerCreation = resolve;
  });
  await page.route("**/api/leads", async (route) => {
    originalBody = route.request().postDataJSON();
    const response = await route.fetch();
    assert.ok(response.ok());
    created = await response.json();
    await route.abort("failed");
    confirmServerCreation();
  });
  await page
    .getByRole("button", { name: "New chat in this project", exact: true })
    .click();
  await serverCreated;
  await page
    .getByRole("button", { name: "Retry chat request", exact: true })
    .waitFor();
  await page.unroute("**/api/leads");
  await page.reload();
  await page
    .getByRole("button", { name: "Retry chat request", exact: true })
    .waitFor();
  const replay = page.waitForResponse(
    (response) =>
      response.url() === url + "/api/leads" &&
      response.request().method() === "POST",
  );
  await page
    .getByRole("button", { name: "Retry chat request", exact: true })
    .click();
  const replayResponse = await replay;
  assert.deepEqual(
    replayResponse.request().postDataJSON(),
    originalBody,
    "Reload retries the exact chat request",
  );
  assert.equal((await replayResponse.json()).id, created.id);
  await page.waitForFunction(
    (id) => JSON.parse(localStorage.getItem("codex-mobile-opened")) === id,
    created.id,
  );
  const serverState = await (await fetch(url + "/api/state")).json();
  assert.equal(
    serverState.threads.filter((item) => item.id === created.id).length,
    1,
  );
  assert.equal(
    await page
      .getByRole("button", { name: "Retry chat request", exact: true })
      .count(),
    0,
  );
  await page.locator("#message").waitFor();
  await page
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  assert.equal(
    await page.getByLabel("Chat account", { exact: true }).isEnabled(),
    true,
  );
  await page.keyboard.press("Escape");
  // Use the real runtime to create another chat while the previous chat is empty.
  const post = async (path, body) => {
    const response = await fetch(url + path, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Canvas-Token": serverState.token,
      },
      body: JSON.stringify(body),
    });
    const result = await response.json();
    assert.ok(response.ok, JSON.stringify(result));
    return result;
  };
  const profile = join(state, "fixture-account");
  await mkdir(profile);
  await writeFile(
    join(profile, "auth.json"),
    JSON.stringify({ OPENAI_API_KEY: "fixture-key-never-sent" }),
  );
  const accounts = await post("/api/accounts/register", { home: profile });
  const alternate = accounts.accounts.find(
    (account) => account.label === "fixture-account",
  );
  assert.ok(alternate);
  await post("/api/agents/account", {
    id: created.id,
    account_key: alternate.id,
  });
  const project = serverState.runtime.projects.find(
    (item) => item.path === created.cwd,
  );
  await post("/api/projects", {
    action: "set_account",
    path: created.cwd,
    account_key: "default",
    expected_revision: project?.accountRevision || 0,
  });
  await page.reload();
  await page
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  await page.getByLabel("Chat account", { exact: true }).waitFor();
  assert.equal(
    await page.getByLabel("Chat account", { exact: true }).inputValue(),
    alternate.id,
  );
  const nextResponse = page.waitForResponse(
    (response) =>
      response.url() === url + "/api/leads" &&
      response.request().method() === "POST",
  );
  await page
    .getByRole("button", { name: "New chat in this project", exact: true })
    .click();
  const next = await nextResponse;
  assert.ok(next.ok());
  const nextRequest = next.request().postDataJSON();
  const nextChat = await next.json();
  assert.equal(nextRequest.reuse_empty, false);
  assert.equal(nextRequest.account_key, undefined);
  assert.equal(nextRequest.previous, created.id);
  assert.equal(nextChat.id, nextRequest.id);
  assert.notEqual(nextChat.id, created.id);
  assert.equal(
    nextChat.accountKey,
    "default",
    "The project account replaces the previous empty chat account",
  );
  await page.waitForFunction(
    (id) => JSON.parse(localStorage.getItem("codex-mobile-opened")) === id,
    nextChat.id,
  );
  assert.equal(
    await page
      .getByRole("button", { name: "Retry chat request", exact: true })
      .count(),
    0,
  );
  await post("/api/projects", { path: profile, account_key: alternate.id });
  await page.reload();
  await page.getByLabel("Toggle conversations").click();
  const otherResponse = page.waitForResponse(
    (response) =>
      response.url() === url + "/api/leads" &&
      response.request().method() === "POST",
  );
  await page
    .getByRole("button", { name: "New chat in fixture-account", exact: true })
    .click();
  const other = await otherResponse;
  assert.ok(other.ok());
  const otherRequest = other.request().postDataJSON();
  const otherChat = await other.json();
  assert.equal(otherRequest.reuse_empty, false);
  assert.equal(otherRequest.previous, nextChat.id);
  assert.equal(otherChat.id, otherRequest.id);
  assert.notEqual(otherChat.id, nextChat.id);
  assert.equal(otherChat.accountKey, alternate.id);
  assert.equal(otherChat.cwd, await realpath(profile));
  await page.waitForFunction(
    (id) => JSON.parse(localStorage.getItem("codex-mobile-opened")) === id,
    otherChat.id,
  );
  assert.equal(
    await page
      .getByRole("button", { name: "Retry chat request", exact: true })
      .count(),
    0,
  );
  // Desktop controls remain available after a viewport change.
  await page.setViewportSize({ width: 1440, height: 960 });
  await page.locator(".workspace-shortcuts").waitFor();
  await page.getByLabel("Add project", { exact: true }).waitFor();
  await page.locator("#messages-toggle").waitFor();
  const manifest = await (await fetch(url + "/manifest.webmanifest")).json();
  assert.equal(manifest.display, "standalone");
  assert.equal(
    await page.locator(".sync-notices [role=alert]").count(),
    0,
    "No sync error in the rendered client",
  );
  assert.equal(errors.length, 0, errors.join("\n"));
  console.log(
    "mobile-client-ui: PASS (320/390/760px, existing-project sessions, settings, Enter, desktop controls)",
  );
} catch (error) {
  console.error(log);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
