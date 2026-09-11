#!/usr/bin/env node
// Production React build and isolated HTTP fixture. No model calls or user state.
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
const measurePanelLayout = createRequire(import.meta.url)(
  "../desktop/panel-layout.cjs",
);
const root = await mkdtemp(join(tmpdir(), "codex-agent-panel-"));
const proc = spawn(
  "python3",
  ["-B", join(repo, "tests/simple-ui-fixture.py"), root],
  {
    stdio: ["pipe", "pipe", "pipe"],
  },
);
let browser,
  page,
  log = "";
proc.stderr.on("data", (data) => (log += data));
const poll = async (fn, label) => {
  for (let n = 0; n < 120; n++) {
    if (await fn()) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(label + " " + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (data) => resolve(Number(String(data).trim())));
    proc.once("exit", () => reject(new Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = await (await fetch(origin + "/api/state")).json();
  const lead = state.threads.find((a) => a.name === "Release lead");
  const other = state.threads.find((a) => a.name === "Other project");
  const panels = new Map();
  const put = (agent, version, html = "", css = "", callbacks = []) =>
    panels.set(agent.id, {
      agent: agent.id,
      version,
      html,
      css,
      callbacks,
      updated: Date.now() / 1000,
    });
  put(lead, 0);
  put(other, 1, "<strong>Other agent panel</strong>");
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  page = await browser.newPage({ viewport: { width: 1280, height: 980 } });
  const errors = [],
    forbidden = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("request", (r) => {
    if (/panel-escape|evil\.invalid/.test(r.url())) forbidden.push(r);
  });
  // This fixture changes panel versions through snapshots, without replication.
  await page.route("**/api/sync/identity", (route) =>
    route.fulfill({ status: 404, json: { error: "Snapshot fixture" } }),
  );
  await page.route("**/api/state", async (route) => {
    const response = await route.fetch();
    const data = await response.json();
    for (const agent of data.threads)
      agent.panelVersion = panels.get(agent.id)?.version || 0;
    data.runtime.requests = [];
    await route.fulfill({ response, json: data });
  });
  let deferred,
    deferLead = false,
    failLead = false;
  await page.route("**/api/panel?*", async (route) => {
    const id = new URL(route.request().url()).searchParams.get("agent");
    if (deferLead && id === lead.id) {
      deferred = route;
      return;
    }
    if (failLead && id === lead.id) {
      await route.fulfill({
        status: 503,
        json: { error: "Fixture unavailable" },
      });
      return;
    }
    await route.fulfill({ json: panels.get(id) });
  });
  let queued = false;
  // Keep transcript and queue snapshots consistent in this panel fixture.
  await page.route("**/api/transcript/stream?*", (route) =>
    route.fulfill({ status: 404, json: { error: "Snapshot fixture" } }),
  );
  await page.route("**/api/transcript?*", async (route) => {
    const response = await route.fetch();
    const data = await response.json();
    const id = new URL(route.request().url()).searchParams.get("id");
    if (queued && id === lead.id)
      data.items.push({
        id: `${lead.id}:queued-fixture`,
        clientMessageId: "queued-fixture",
        role: "user",
        text: "A queued message",
        deliveryStatus: "pending",
        delivery: "queue",
      });
    await route.fulfill({ response, json: data });
  });
  await page.route("**/api/queue?*", async (route) => {
    await route.fulfill({
      json: {
        items: queued
          ? [{ id: "queued-fixture", text: "A queued message" }]
          : [],
      },
    });
  });
  const submissions = [];
  let callbackMode = "success";
  let pendingCallback;
  await page.route("**/api/panel/callback", async (route) => {
    const body = route.request().postDataJSON();
    submissions.push(body);
    assert.ok(route.request().headers()["x-canvas-token"]);
    if (callbackMode === "lost") {
      await route.abort("failed");
      return;
    }
    if (callbackMode === "defer") {
      pendingCallback = route;
      return;
    }
    if (callbackMode === "stale") {
      await route.fulfill({
        status: 409,
        json: { error: "Panel changed. Use the latest panel." },
      });
      return;
    }
    await route.fulfill({ json: { ...body, status: "pending" } });
  });
  await page.goto(origin);
  const select = (name) =>
    page.locator("[data-chat]").filter({ hasText: name }).click();
  await select("Release lead");
  const panel = page.locator(".agent-panel");
  await panel.waitFor({ state: "hidden" });
  const assertLayout = async () => {
    await page.waitForFunction(() => {
      const panel = document.querySelector(".agent-panel");
      return (
        panel?.dataset.ready === "true" &&
        panel.getBoundingClientRect().height > 0 &&
        Math.abs(
          panel.getBoundingClientRect().height - parseFloat(panel.style.height),
        ) < 0.5
      );
    });
    const bounds = await panel.boundingBox();
    const transcript = await page.locator("#messages").boundingBox();
    const composer = await page.locator("#composer").boundingBox();
    assert.ok(
      bounds.height > 0 && bounds.height <= 150,
      JSON.stringify(bounds),
    );
    assert.equal((await panel.locator("iframe").boundingBox()).height, 150);
    assert.equal(
      await panel.evaluate((el) => el.nextElementSibling?.id),
      "composer",
    );
    assert.ok(transcript.y + transcript.height <= bounds.y + 1);
    assert.ok(bounds.y + bounds.height <= composer.y + 1);
    assert.ok(
      bounds.x >= 0 && bounds.x + bounds.width <= page.viewportSize().width + 1,
    );
  };
  assert.ok(
    (await page.locator("#composer").boundingBox()).height <= 100,
    "The empty composer must stay compact",
  );
  put(
    lead,
    1,
    '<div class="progress"><span>Review 12 of 40</span><svg width="160" height="20"><rect width="90" height="10" fill="#9c92ff"/></svg></div>',
    ".progress{padding:16px;border-radius:10px;background:rgb(40,42,60);display:flex;gap:20px}",
  );
  const frame = panel.frameLocator("iframe");
  await frame.getByText("Review 12 of 40").waitFor();
  await assertLayout();
  assert.equal(
    await frame
      .locator(".progress")
      .evaluate((el) => getComputedStyle(el).backgroundColor),
    "rgb(40, 42, 60)",
  );
  assert.equal(await frame.locator("svg rect").count(), 1);
  assert.equal(
    await page.locator("#messages").getByText("Review 12 of 40").count(),
    0,
  );
  put(lead, 2, '<p id="progress">Review 28 of 40</p>');
  await frame.getByText("Review 28 of 40").waitFor();
  assert.equal(await frame.getByText("Review 12 of 40").count(), 0);
  deferLead = true;
  put(lead, 3, "<p>Late lead result</p>");
  const beforeRefresh = await page.locator("#composer").boundingBox();
  await poll(() => !!deferred, "lead refresh starts");
  assert.equal(await frame.getByText("Review 28 of 40").count(), 1);
  assert.deepEqual(
    await page.locator("#composer").boundingBox(),
    beforeRefresh,
    "a delayed panel revision does not move the composer",
  );
  await select("Other project");
  await frame.getByText("Other agent panel").waitFor();
  await deferred.fulfill({ json: panels.get(lead.id) }).catch(() => {});
  await page.waitForTimeout(150);
  assert.equal(await frame.getByText("Late lead result").count(), 0);
  assert.equal(await panel.getAttribute("data-agent"), other.id);
  deferLead = false;
  await select("Release lead");
  await frame.getByText("Late lead result").waitFor();
  put(lead, 4);
  await panel.waitFor({ state: "hidden" });
  assert.equal(await panel.count(), 0);
  put(
    lead,
    5,
    `<meta http-equiv="refresh" content="0;url=https://evil.invalid/panel-escape"><meta http-equiv="Content-Security-Policy" content="default-src * 'unsafe-inline'"><script>parent.document.body.dataset.panelEscape='yes';fetch('/api/panel-escape')</script><p>Isolated panel</p><a href="https://evil.invalid/panel-escape" target="_top">Escape</a><form action="/api/panel-escape"><button>Submit</button></form><img src="https://evil.invalid/panel-escape"><div class="remote">Remote CSS</div><div style="width:3000px;height:450px">Scrollable content</div>`,
    `.remote{background:url(https://evil.invalid/panel-escape)}\n</style><meta http-equiv="refresh" content="0;url=https://evil.invalid/panel-escape"><script>parent.document.body.dataset.panelEscape='yes'</script><style>`,
  );
  await frame.getByText("Isolated panel").waitFor();
  assert.equal(
    await panel.locator("iframe").getAttribute("sandbox"),
    "allow-scripts allow-forms",
  );
  assert.equal(
    await panel.locator("iframe").getAttribute("referrerpolicy"),
    "no-referrer",
  );
  assert.equal(
    await frame.locator("iframe,meta[http-equiv=refresh]").count(),
    0,
  );
  assert.equal(
    await frame.locator('meta[http-equiv="Content-Security-Policy"]').count(),
    1,
  );
  assert.equal(
    await frame.getByText("Escape", { exact: true }).getAttribute("href"),
    null,
  );
  await frame.getByText("Escape", { exact: true }).click();
  assert.equal(
    await frame.getByRole("button", { name: "Submit" }).isDisabled(),
    true,
  );
  await frame
    .locator("form")
    .evaluate((form) => HTMLFormElement.prototype.requestSubmit.call(form));
  await page.waitForTimeout(250);
  assert.equal(page.url(), origin + "/");
  assert.equal(submissions.length, 0, "Unbound form cannot send or navigate");
  assert.equal(
    await page.locator("body").getAttribute("data-panel-escape"),
    null,
  );
  assert.ok(
    forbidden.every((request) =>
      ["csp", "net::ERR_BLOCKED_BY_CSP"].includes(request.failure()?.errorText),
    ),
    "resource requests must be blocked by CSP: " +
      JSON.stringify(
        forbidden.map((request) => ({
          url: request.url(),
          failure: request.failure(),
        })),
      ),
  );
  // The fixed-height document root can hide overflow in its body. Validate the
  // same rendered geometry used by native preflight, not the root's scroll box.
  const overflow = await frame.locator("body").evaluate(measurePanelLayout);
  assert.equal(overflow.fits, false);
  assert.equal(overflow.height, 150);
  assert.ok(overflow.contentHeight > overflow.height);
  assert.ok(overflow.contentWidth > overflow.width);
  assert.ok(
    overflow.violations.some((item) =>
      ["outside-panel", "clipped-content"].includes(item.kind),
    ),
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await assertLayout();
  await page.screenshot({ path: join(root, "panel-mobile.png") });
  await page.setViewportSize({ width: 1280, height: 980 });
  failLead = true;
  put(lead, 6, "<p>Unavailable</p>");
  await panel.getByRole("alert").waitFor();
  assert.match(
    await panel.getByRole("alert").textContent(),
    /Fixture unavailable/,
  );
  assert.equal(
    await frame.getByText("Isolated panel").count(),
    1,
    "last good contents remain during failure",
  );
  failLead = false;
  await panel.getByRole("button", { name: "Retry", exact: true }).click();
  await frame.getByText("Unavailable", { exact: true }).waitFor();
  assert.equal(await panel.getByRole("alert").count(), 0);
  assert.equal(
    await panel.getAttribute("data-panel-version"),
    "6",
    "retry recovers the same version",
  );
  put(
    lead,
    7,
    '<div class="progress"><h3>Release review</h3><p>28 of 40 agents complete</p><progress value="28" max="40"></progress></div>',
    ".progress{padding:12px 24px}h3{font-size:17px;margin:0}p{color:#9696a5}progress{width:100%;accent-color:#a399ff}",
  );
  await frame.getByText("28 of 40 agents complete").waitFor();
  await page.screenshot({ path: join(root, "panel-desktop.png") });
  const callbacks = [
    {
      id: "literal",
      label: "Literal </script><script>alert(1)</script>",
      fields: [],
    },
    { id: "approve", label: "Approve", fields: [] },
    {
      id: "choose",
      label: "Choose",
      fields: ["choice", "note", "submit", "reportValidity"],
    },
  ];
  const interactive = `<h3>Review actions</h3><button data-callback="approve">Approve release</button><form id="choose-form" data-callback="choose"><label>Note <input name="note" required></label><select name="choice" multiple><option selected value="a">A</option><option selected value="b">B</option></select><input type="hidden" name="reportValidity" value="safe"><input type="hidden" name="secret" value="not sent"><input type="file" name="upload" style="display:none"></form><button form="choose-form" name="submit" value="yes">Send choice</button>`;
  queued = true;
  put(lead, 8, interactive, "", callbacks);
  await frame.getByRole("button", { name: "Approve release" }).waitFor();
  const queuedMessage = page
    .locator("#messages .message.user")
    .filter({ hasText: "A queued message" });
  await queuedMessage
    .getByRole("button", { name: "Edit queued message", exact: true })
    .waitFor();
  assert.equal(
    await queuedMessage
      .getByRole("button", { name: "Cancel queued message", exact: true })
      .count(),
    1,
  );
  assert.equal(
    await page.getByText("A queued message", { exact: true }).count(),
    1,
  );
  assert.equal(await page.locator(".message-queue").count(), 0);
  await page.waitForTimeout(400);
  assert.equal(submissions.length, 0, "render never sends a callback");
  await assertLayout();
  assert.equal(await frame.locator("input[type=file]").isDisabled(), true);
  assert.equal(
    await frame.locator("html").evaluate((el) => getComputedStyle(el).color),
    "rgb(233, 233, 238)",
  );
  assert.equal(
    await frame
      .getByRole("button", { name: "Approve release" })
      .evaluate((el) => getComputedStyle(el).borderRadius),
    "8px",
  );
  // Source spoofing with the correct public document channel still has the wrong Window.
  const channel = await panel
    .locator("iframe")
    .evaluate(
      (el) =>
        JSON.parse(
          decodeURIComponent(
            new DOMParser()
              .parseFromString(el.srcdoc, "text/html")
              .querySelector("script")
              .getAttribute("data-config"),
          ),
        ).channel,
    );
  await page.evaluate(
    (channel) =>
      window.postMessage(
        { type: "panel-callback", channel, callback: "approve", values: {} },
        "*",
      ),
    channel,
  );
  await frame
    .getByRole("button", { name: "Approve release" })
    .evaluate((el) => el.click());
  await page.waitForTimeout(150);
  assert.equal(
    submissions.length,
    0,
    "synthetic click and source spoof cannot callback",
  );
  callbackMode = "defer";
  const beforeCallback = await page.locator("#messages").boundingBox();
  const composerBeforeCallback = await page.locator("#composer").boundingBox();
  await frame.getByRole("button", { name: "Approve release" }).click();
  await poll(() => submissions.length === 1, "trusted button callback");
  assert.equal(
    await frame.getByRole("button", { name: "Approve release" }).isDisabled(),
    true,
  );
  assert.equal(
    await frame.getByRole("button", { name: "Send choice" }).isDisabled(),
    true,
  );
  await pendingCallback.fulfill({
    json: { ...submissions[0], status: "pending" },
  });
  await page
    .locator(".agent-panel-feedback")
    .getByText("Approve: sent to agent")
    .waitFor();
  await assertLayout();
  const feedbackBounds = await page
    .locator(".agent-panel-feedback")
    .boundingBox();
  const panelBounds = await panel.boundingBox();
  assert.ok(feedbackBounds.y >= panelBounds.y);
  assert.ok(
    feedbackBounds.y + feedbackBounds.height <=
      panelBounds.y + panelBounds.height,
  );
  assert.deepEqual(
    await page.locator("#messages").boundingBox(),
    beforeCallback,
    "callback feedback does not shrink or move the transcript",
  );
  assert.deepEqual(
    await page.locator("#composer").boundingBox(),
    composerBeforeCallback,
    "callback feedback does not move the composer",
  );
  await page.getByRole("button", { name: "Dismiss panel notice" }).click();
  assert.equal(await page.locator(".agent-panel-feedback").count(), 0);
  assert.deepEqual(
    await page.locator("#messages").boundingBox(),
    beforeCallback,
  );
  callbackMode = "lost";
  await frame.getByRole("button", { name: "Send choice" }).click();
  await page.waitForTimeout(100);
  assert.equal(
    submissions.length,
    1,
    "required native form field blocks empty submit",
  );
  await frame.getByRole("textbox", { name: "Note" }).fill("Review it");
  await frame.getByRole("button", { name: "Send choice" }).click();
  await page.locator('.agent-panel-feedback[role="alert"]').waitFor();
  assert.deepEqual(submissions[1].values, {
    choice: ["a", "b"],
    note: ["Review it"],
    submit: ["yes"],
    reportValidity: ["safe"],
  });
  await select("Other project");
  await frame.getByText("Other agent panel").waitFor();
  await select("Release lead");
  await page
    .locator(".agent-panel-feedback")
    .getByRole("button", { name: "Retry", exact: true })
    .waitFor();
  assert.equal(submissions.length, 2, "remount never retries automatically");
  callbackMode = "success";
  await page
    .locator(".agent-panel-feedback")
    .getByRole("button", { name: "Retry", exact: true })
    .click();
  await page
    .locator(".agent-panel-feedback")
    .getByText("Choose: sent to agent")
    .waitFor();
  assert.deepEqual(
    submissions[2],
    submissions[1],
    "lost response retry preserves body and identity",
  );
  assert.equal(
    await frame.getByRole("button", { name: "Send choice" }).isDisabled(),
    true,
  );
  put(lead, 9, interactive, "button{border-radius:2px}", callbacks);
  await poll(
    async () => (await panel.getAttribute("data-panel-version")) === "9",
    "new version unlocks actions",
  );
  assert.equal(
    await frame.getByRole("button", { name: "Approve release" }).isDisabled(),
    false,
  );
  assert.equal(
    await frame
      .getByRole("button", { name: "Approve release" })
      .evaluate((el) => getComputedStyle(el).borderRadius),
    "2px",
  );
  callbackMode = "stale";
  await frame.getByRole("button", { name: "Approve release" }).click();
  await page
    .locator(".agent-panel-feedback")
    .getByText("Panel changed. Use the latest panel.")
    .waitFor();
  assert.equal(
    await page
      .locator(".agent-panel-feedback")
      .getByRole("button", { name: "Retry", exact: true })
      .count(),
    0,
  );
  assert.equal(
    await frame.getByRole("button", { name: "Approve release" }).isDisabled(),
    true,
  );
  put(lead, 10, interactive, "", callbacks);
  panels.get(lead.id).submittedCallbacks = ["approve"];
  await poll(
    async () => (await panel.getAttribute("data-panel-version")) === "10",
    "server accepted actions loaded",
  );
  assert.equal(
    await frame.getByRole("button", { name: "Approve release" }).isDisabled(),
    true,
  );
  deferred = undefined;
  deferLead = true;
  put(lead, 11, interactive, "", callbacks);
  await poll(() => !!deferred, "next panel fetch");
  await poll(
    async () =>
      await frame.getByRole("button", { name: "Send choice" }).isDisabled(),
    "old actions disabled during version fetch",
  );
  deferLead = false;
  await deferred.fulfill({ json: panels.get(lead.id) }).catch(() => {});
  await poll(
    async () => (await panel.getAttribute("data-panel-version")) === "11",
    "latest panel loaded",
  );
  callbackMode = "success";
  const beforeEnter = submissions.length;
  await frame.getByRole("textbox", { name: "Note" }).fill("Keyboard submit");
  await frame.getByRole("textbox", { name: "Note" }).press("Enter");
  await poll(
    () => submissions.length === beforeEnter + 1,
    "Enter submits the registered form",
  );
  await page
    .locator(".agent-panel-feedback")
    .getByText("Choose: sent to agent")
    .waitFor();
  assert.deepEqual(submissions.at(-1).values.note, ["Keyboard submit"]);
  await assertLayout();
  await page.screenshot({ path: join(root, "panel-interactive.png") });
  await page.setViewportSize({ width: 320, height: 640 });
  await panel.waitFor();
  await assertLayout();
  const composer = await page.locator("#composer").boundingBox();
  assert.ok(
    composer.y >= 0 && composer.y + composer.height <= 640,
    "The message field remains visible with the mobile agent panel",
  );
  await page.waitForTimeout(200);
  await page.screenshot({ path: join(root, "panel-mobile.png") });
  await page.setViewportSize({ width: 1280, height: 980 });
  await page.locator("#messages-toggle").click();
  const messages = page.getByRole("dialog", { name: "Messages", exact: true });
  await messages.getByRole("tab", { name: "Team", exact: true }).click();
  await messages.locator("[data-room]").first().click();
  await page.locator(".team-room-footer").waitFor();
  assert.equal(
    await messages.locator(".agent-panel").count(),
    0,
  );
  assert.deepEqual(errors, []);
  console.log(
    JSON.stringify({
      passed: true,
      height: 150,
      update: true,
      clear: true,
      isolated: true,
      lateResponse: true,
      errors: true,
      mobile: true,
      rooms: true,
      callbacks: true,
      exactRetry: true,
      enterSubmit: true,
      screenshots: root,
    }),
  );
} catch (error) {
  await page?.screenshot({ path: join(root, "failure.png") });
  console.error("Failure evidence:", root);
  throw error;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null)
    await new Promise((resolve) => proc.once("exit", resolve));
}
