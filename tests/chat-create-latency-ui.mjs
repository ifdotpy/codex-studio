#!/usr/bin/env node
// Confirmed creation opens before the full chat projection arrives. No model calls.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
const repo = dirname(dirname(fileURLToPath(import.meta.url)));
const { chromium } = createRequire(join(repo, "web/package.json"))(
  "playwright-core",
);
const evidence = await mkdtemp(join(tmpdir(), "studio-chat-create-"));
const fixture = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), evidence],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let browser,
  page,
  log = "";
fixture.stderr.on("data", (data) => {
  log += data;
});
try {
  const port = await new Promise((resolve, reject) => {
    fixture.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    fixture.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const initial = await (await fetch(origin + "/api/state?view=chat")).json();
  const otherHome = join(evidence, "alternate-account");
  await mkdir(otherHome);
  await writeFile(
    join(otherHome, "auth.json"),
    JSON.stringify({
      tokens: { account_id: "draft-account", access_token: "fixture" },
    }),
  );
  const registration = await fetch(origin + "/api/accounts/register", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": initial.token,
    },
    body: JSON.stringify({ home: otherHome }),
  });
  assert.equal(registration.ok, true);
  const registered = await registration.json();
  const destination = registered.accounts.find(
    (account) => account.label === "alternate-account",
  );
  assert.ok(destination);
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  });
  page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/sync/**", (route) =>
    route.fulfill({ status: 404, json: { error: "HTTP fixture" } }),
  );
  const accountChanges = [];
  const transfers = [];
  page.on("request", (request) => {
    if (request.url().endsWith("/api/agents/account"))
      accountChanges.push(request.postDataJSON());
    if (request.url().endsWith("/api/agents/account-transfer"))
      transfers.push(request.postDataJSON());
  });
  let hold = false,
    stale = true,
    confirmed,
    request,
    repliedAt;
  const pending = [];
  await page.route(/\/api\/state(?:\?.*)?$/, (route) => {
    if (hold) {
      pending.push(route);
      return;
    }
    return stale ? route.fulfill({ json: initial }) : route.continue();
  });
  await page.route("**/api/leads", async (route) => {
    request = route.request().postDataJSON();
    assert.equal(
      Object.hasOwn(request, "model"),
      false,
      "default creation does not request model metadata",
    );
    const response = await route.fetch();
    assert.equal(response.ok(), true);
    confirmed = await response.json();
    assert.equal(confirmed.model, "gpt-6-astra");
    assert.equal(
      confirmed.empty,
      true,
      "creation receipt includes account eligibility before the snapshot",
    );
    assert.equal(
      confirmed.threadId,
      null,
      "blank creation does not start Codex",
    );
    hold = true;
    repliedAt = Date.now();
    await route.fulfill({ response });
  });
  await page.goto(origin);
  await page.locator("[data-chat]").filter({ hasText: "Release lead" }).click();
  await page.locator("#message").fill("Keep the previous draft");
  await page.locator(".project-tree-heading").first().hover();
  await page
    .getByRole("button", { name: /^New chat in / })
    .first()
    .click();
  await page
    .locator("#conversation-title")
    .getByText("New chat", { exact: true })
    .waitFor({ timeout: 2000 });
  const openedAfterMs = Date.now() - repliedAt;
  assert.ok(
    openedAfterMs < 1500,
    `creation opens before refresh: ${openedAfterMs}ms`,
  );
  assert.equal(await page.locator("#message").isEnabled(), true);
  assert.equal(await page.locator("#message").inputValue(), "");
  await page.locator("#message").fill("New draft before the projection");
  assert.equal(await page.locator("#send").isEnabled(), true);
  await page.screenshot({ path: join(evidence, "created-before-refresh.png") });
  await page
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  await page.locator(".account-picker").click();
  await page
    .getByText("Account for this conversation", { exact: true })
    .waitFor();
  assert.equal(
    await page.getByText("Transfer chat to account", { exact: true }).count(),
    0,
  );
  const accountResponse = page.waitForResponse((response) =>
    response.url().endsWith("/api/agents/account"),
  );
  const accountStartedAt = Date.now();
  await page
    .getByRole("menuitem")
    .filter({ hasText: destination.email || destination.label })
    .click();
  const selectedResponse = await accountResponse;
  assert.equal(selectedResponse.ok(), true);
  const selected = await selectedResponse.json();
  assert.equal(selected.empty, true);
  assert.equal(selected.threadId, null);
  const accountChangeMs = Date.now() - accountStartedAt;
  await page
    .locator(".account-picker")
    .getByText(destination.email || destination.label, { exact: true })
    .waitFor();
  assert.equal(accountChanges.length, 1);
  assert.equal(accountChanges[0].id, confirmed.id);
  assert.equal(
    transfers.length,
    0,
    "a draft account choice never starts a transfer",
  );
  await page.keyboard.press("Escape");
  hold = false;
  for (const route of pending.splice(0)) await route.fulfill({ json: initial });
  await page.waitForTimeout(1800);
  assert.equal(
    await page.locator("#conversation-title").innerText(),
    "New chat",
  );
  assert.equal(
    await page.locator("#message").inputValue(),
    "New draft before the projection",
  );
  assert.equal(
    await page.locator(`[data-chat="${confirmed.id}"]`).count(),
    1,
    "stale projections retain one confirmed chat",
  );
  // A reload can read the same old snapshot before replication reaches this chat.
  await page.reload();
  await page.locator(`#chat-list [data-chat="${confirmed.id}"]`).waitFor();
  assert.equal(
    await page.locator("#conversation-title").innerText(),
    "New chat",
  );
  assert.equal(
    await page.locator("#message").inputValue(),
    "New draft before the projection",
  );
  stale = false;
  await page.waitForTimeout(1800);
  assert.equal(
    await page.locator(`[data-chat="${confirmed.id}"]`).count(),
    1,
    "the final projection does not duplicate the chat",
  );
  assert.equal(
    await page.locator("#message").inputValue(),
    "New draft before the projection",
  );
  const final = await (await fetch(origin + "/api/state?view=chat")).json();
  assert.equal(
    final.runtime.agents.filter((a) => a.id === request.id).length,
    1,
  );
  await page.waitForFunction(
    () =>
      JSON.parse(localStorage.getItem("codex-confirmed-chats")).agents
        .length === 0,
  );
  stale = true;
  await page.locator(".project-tree-heading").first().hover();
  await page
    .getByRole("button", { name: /^New chat in / })
    .first()
    .click();
  await page.waitForFunction(
    () =>
      JSON.parse(localStorage.getItem("codex-confirmed-chats")).agents
        .length === 1,
  );
  const deletedId = confirmed.id;
  hold = false;
  for (const route of pending.splice(0)) await route.fulfill({ json: initial });
  const row = page.locator(".sidebar-row").filter({
    has: page.locator(`[data-chat="${deletedId}"]`),
  });
  await row.hover();
  await row.locator(".row-actions").click();
  await page.getByRole("menuitem", { name: "Delete", exact: true }).click();
  await page.locator("[data-delete-chat]").click();
  await page
    .locator(`[data-chat="${deletedId}"]`)
    .waitFor({ state: "detached" });
  await page.reload();
  await page.locator("[data-chat]").first().waitFor();
  assert.equal(
    await page.locator(`[data-chat="${deletedId}"]`).count(),
    0,
    "deletion before the projection cannot restore a confirmed chat",
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({ passed: true, openedAfterMs, accountChangeMs, evidence }),
  );
} catch (error) {
  await page?.screenshot({ path: join(evidence, "failure.png") });
  console.error("Evidence:", evidence);
  throw error;
} finally {
  await browser?.close();
  fixture.kill("SIGTERM");
}
