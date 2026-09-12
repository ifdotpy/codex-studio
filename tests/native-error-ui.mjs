#!/usr/bin/env node
// Real app-server notification handlers, SQLite, event stream, and browser. No inference.
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
const root = await mkdtemp(join(tmpdir(), "codex-stream-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["pipe", "pipe", "pipe"] },
);
let log = "",
  browser,
  page;
proc.stderr.on("data", (d) => (log += d));
const poll = async (fn, label) => {
  for (let i = 0; i < 100; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  throw Error(label + " " + log);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(log)));
  });
  const origin = `http://127.0.0.1:${port}`;
  const state = async () => await (await fetch(origin + "/api/state")).json();
  browser = await chromium.launch({
    executablePath:
      process.env.CHROME_BIN ||
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    headless: true,
  });
  page = await browser.newPage({ viewport: { width: 1200, height: 900 } });
  const errors = [];
  let limitReads = 0;
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (message) => {
    if (
      message.type() === "error" &&
      message.text().includes("Encountered two children")
    )
      errors.push(message.text());
  });
  page.on("request", (r) => {
    if (new URL(r.url()).pathname === "/api/limits") limitReads++;
  });
  if (process.env.NATIVE_ERROR_LEGACY) {
    await page.route("**/api/sync/identity", (r) =>
      r.fulfill({ status: 404, json: { error: "Unsupported sync" } }),
    );
  }
  await page.route("**/api/limits", (route) =>
    route.fulfill({
      json: {
        accountKey: "default",
        at: Date.now() / 1000,
        data: {
          rateLimits: {
            planType: "pro",
            rateLimitReachedType: "workspace_member_credits_depleted",
          },
        },
      },
    }),
  );
  proc.stdin.write(
    JSON.stringify({
      method: "fixture/limits",
      params: {
        rateLimits: {
          planType: "pro",
          rateLimitReachedType: "workspace_member_credits_depleted",
        },
      },
    }) + "\n",
  );
  await page.goto(origin);
  await page
    .locator("[data-chat]")
    .filter({ hasText: "Other project" })
    .click();
  await page.locator("#message").fill("Exercise the live stream");
  await page.locator("#send").click();
  let agent;
  await poll(async () => {
    agent = (await state()).runtime.agents.find(
      (a) => a.name === "Other project",
    );
    return agent.status === "running" && agent.turnId;
  }, "turn starts");
  const event = (method, params) =>
    proc.stdin.write(
      JSON.stringify({
        method,
        params: { threadId: agent.threadId, turnId: agent.turnId, ...params },
      }) + "\n",
    );
  const accountDetails = {
    code: "fixture_configuration",
    files: ["first.toml", "second.toml"],
    retryAllowed: false,
  };
  event("configWarning", {
    threadId: undefined,
    turnId: undefined,
    message: "Account configuration needs review",
    details: accountDetails,
  });
  const accountNotice = page.locator(".native-account-notices");
  await accountNotice.locator("summary").click();
  assert.deepEqual(
    JSON.parse(await accountNotice.locator("pre").innerText()),
    accountDetails,
    "Account-wide native diagnostics retain object details without removing the UI",
  );
  assert.equal(
    await page.locator("#conversation-title").innerText(),
    "Other project",
  );
  event("model/safetyBuffering/updated", {
    model: "gpt-6-astra",
    showBufferingUi: true,
    fasterModel: "gpt-5.6-sol",
    useCases: ["cyber"],
    reasons: ["review"],
  });
  await page.locator('[data-safety-state="waiting"]').waitFor();
  await page
    .getByRole("button", { name: "Retry with a faster model", exact: true })
    .click();
  await page
    .getByRole("dialog")
    .getByText("Codex offers gpt-5.6-sol.", { exact: false })
    .waitFor();
  await page.waitForTimeout(250); // Let the dialog enter before the visual capture.
  await page.screenshot({ path: join(root, "native-safety-confirm.png") });
  await page
    .getByRole("dialog")
    .getByRole("button", { name: "Keep waiting", exact: true })
    .click();
  await page.getByRole("dialog").waitFor({ state: "detached" });
  await page.setViewportSize({ width: 390, height: 844 });
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 390);
  await page.screenshot({ path: join(root, "native-safety-mobile.png") });
  await page
    .locator(".native-safety")
    .getByRole("button", { name: "Keep waiting", exact: true })
    .click();
  await page.locator(".native-safety").waitFor({ state: "detached" });
  await page.setViewportSize({ width: 1200, height: 900 });
  event("model/safetyBuffering/updated", {
    model: "gpt-6-astra",
    showBufferingUi: false,
  });
  event("error", {
    willRetry: true,
    error: {
      message: "Reconnecting 1/5",
      codexErrorInfo: "other",
      additionalDetails:
        "The request failed. Another attempt starts in 2 seconds.",
    },
  });
  await page.locator('[data-phase="retrying"]').waitFor();
  assert.equal(await page.locator(".native-error").count(), 0);
  await page
    .locator('[data-phase="retrying"]')
    .getByText("Reconnecting 1/5", { exact: true })
    .waitFor();
  await page
    .locator('[data-phase="retrying"] pre')
    .getByText("The request failed. Another attempt starts in 2 seconds.", {
      exact: true,
    })
    .waitFor();
  event("error", {
    willRetry: true,
    error: {
      message: { message: "Reconnecting 2/5" },
      codexErrorInfo: "other",
      additionalDetails: {
        message: "The request failed. Another attempt starts in 2 seconds.",
        httpStatusCode: 503,
      },
    },
  });
  await page
    .locator('[data-phase="retrying"]')
    .getByText("Reconnecting 2/5", { exact: true })
    .waitFor();
  assert.deepEqual(
    JSON.parse(await page.locator('[data-phase="retrying"] pre').innerText()),
    {
      message: "The request failed. Another attempt starts in 2 seconds.",
      httpStatusCode: 503,
    },
  );
  assert.equal(
    await page.locator("#conversation-status").innerText(),
    "Reconnecting 2/5",
  );
  event("item/agentMessage/delta", {
    itemId: "native-text",
    delta: "Connection restored",
  });
  await page.locator('[data-phase="writing"]').waitFor();
  event("modelProvider/authRecoveryStarted", {
    provider: "test",
    message: "Restoring sign-in",
  });
  await page.locator('[data-phase="auth"]').waitFor();
  event("modelProvider/authRecoveryCompleted", {
    provider: "test",
    message: "Sign-in restored",
  });
  await page.locator('[data-phase="thinking"]').waitFor();
  const run = {
    id: "hook-visible",
    eventName: "preToolUse",
    handlerType: "command",
    executionMode: "sync",
    scope: "turn",
    sourcePath: "/fixture/hook",
    source: "project",
    displayOrder: 1,
    status: "running",
    statusMessage: null,
    startedAt: Date.now(),
    completedAt: null,
    durationMs: null,
    entries: [],
  };
  event("hook/started", { run });
  await page
    .locator(".native-notice")
    .getByText("Hook running: preToolUse", { exact: true })
    .waitFor();
  event("hook/completed", {
    run: {
      ...run,
      status: "failed",
      statusMessage: "Hook process exited with code 1",
      entries: [
        { kind: "error", text: "The hook cannot read its configuration." },
      ],
    },
  });
  const hook = page
    .locator(".native-notice.error")
    .filter({ hasText: "Hook failed: preToolUse" });
  await hook.getByText("Hook failed: preToolUse", { exact: true }).click();
  await hook.getByText(/The hook cannot read its configuration/).waitFor();
  assert.equal(
    (await state()).runtime.agents.find((a) => a.id === agent.id).status,
    "running",
  );
  event("hook/started", { run: { ...run, id: "hook-quiet" } });
  await page
    .locator(".native-notice")
    .getByText("Hook running: preToolUse", { exact: true })
    .waitFor();
  event("hook/completed", {
    run: {
      ...run,
      id: "hook-quiet",
      status: "completed",
      entries: [{ kind: "context", text: "Hidden context" }],
    },
  });
  await page
    .locator(".native-notice")
    .getByText("Hook running: preToolUse", { exact: true })
    .waitFor({ state: "detached" });
  assert.equal(
    await page.getByText("Hidden context", { exact: true }).count(),
    0,
  );
  const error = {
    message: "Your session has expired.",
    codexErrorInfo: "unauthorized",
    additionalDetails: "x".repeat(8000),
  };
  event("error", { willRetry: false, error });
  await page
    .locator(".native-error")
    .getByText("Your session has expired.", { exact: true })
    .waitFor();
  await page
    .locator(".native-error")
    .getByText("Sign in again to this chat's account.", { exact: true })
    .waitFor();
  assert.equal(
    await page.locator(".native-error details").getAttribute("open"),
    null,
  );
  event("turn/completed", {
    turn: { id: agent.turnId, status: "failed", error: null },
  });
  await poll(
    async () =>
      (await state()).runtime.agents.find((a) => a.id === agent.id)?.status ===
      "failed",
    "failed native turn",
  );
  // The error card is the failure indicator. A second Failed row is intentionally hidden.
  assert.equal(await page.locator('[data-phase="failed"]').count(), 0);
  await page
    .locator(".native-notice.error")
    .filter({ hasText: error.message })
    .waitFor();
  assert.equal(
    await page
      .locator(".tool-card")
      .filter({ hasText: "Your session has expired" })
      .count(),
    0,
  );
  await page.screenshot({ path: join(root, "native-error-desktop.png") });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator(".native-error summary").click();
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 390);
  const box = await page.locator(".native-error pre").boundingBox();
  assert.ok(box.height <= 181, "error details have bounded height");
  await page.screenshot({ path: join(root, "native-error-mobile.png") });
  const start = async (text) => {
    await page.setViewportSize({ width: 1200, height: 900 });
    await page.locator("#message").fill(text);
    await page.locator("#send").click();
    const previous = agent.turnId;
    await poll(async () => {
      agent = (await state()).runtime.agents.find((a) => a.id === agent.id);
      return agent.inFlight && agent.turnId && agent.turnId !== previous;
    }, "new test turn starts");
  };
  const fail = (failure) => {
    event("error", { willRetry: false, error: failure });
    event("turn/completed", {
      turn: { id: agent.turnId, status: "failed", error: failure },
    });
  };
  await start("Check capacity presentation");
  await page.locator(".native-error").waitFor({ state: "detached" });
  await page
    .locator(".native-notice.error")
    .filter({ hasText: error.message })
    .getByText("Sign in again to this chat's account.", { exact: true })
    .waitFor();
  fail({ message: "Model is at capacity", codexErrorInfo: "serverOverloaded" });
  await page
    .locator(".native-error.warning")
    .getByText("Model is at capacity", { exact: true })
    .waitFor();
  await page
    .locator(".native-notice.warning")
    .getByText("Model is at capacity", { exact: true })
    .waitFor();
  await page
    .locator('.capacity-retry[data-retry-status="scheduled"]')
    .waitFor();
  await page
    .getByRole("button", { name: "Cancel automatic retry", exact: true })
    .click();
  await page.getByText("Automatic retry cancelled.", { exact: true }).waitFor();
  assert.equal(
    (await state()).runtime.agents.find((a) => a.id === agent.id).inFlight,
    false,
  );
  await start("Check limit recovery");
  const beforeLimitError = limitReads;
  fail({
    message: "Usage limit reached",
    codexErrorInfo: "usageLimitExceeded",
  });
  await poll(
    () => limitReads > beforeLimitError,
    "native limit failure refreshes account limits",
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await page
    .getByRole("button", { name: "View account limits", exact: true })
    .click();
  await page
    .getByRole("region", { name: "Account limits details", exact: true })
    .waitFor();
  await page
    .getByRole("region", { name: "Account limits details", exact: true })
    .getByText("Ask your workspace owner to increase your usage limit.", {
      exact: true,
    })
    .waitFor();
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 390);
  await page.screenshot({
    path: join(root, "native-limit-recovery-mobile.png"),
  });
  await page.keyboard.press("Escape");
  for (const [failure, trusted] of [
    [
      { message: "Cyber access denied", codexErrorInfo: "cyberPolicy" },
      "https://chatgpt.com/cyber/",
    ],
    [
      {
        message:
          "HTTP 400: " +
          JSON.stringify({
            error: { code: "bio_policy", message: "Restricted" },
          }),
        codexErrorInfo: "other",
      },
      "https://chatgpt.com/r/b749fb02595e04c3007a54375f3f4374",
    ],
  ]) {
    await start("Check policy explanation");
    fail(failure);
    await page
      .locator(".native-error.info")
      .getByText("This content can't be shown", { exact: true })
      .waitFor();
    await poll(
      async () =>
        (await page
          .locator(".native-error")
          .getByRole("link", { name: "Trusted Access", exact: true })
          .getAttribute("href")) === trusted,
      "policy-specific Trusted Access link",
    );
    assert.equal(
      await page.locator("#message").isEnabled(),
      true,
      "Access errors do not impose a misalignment thread block",
    );
  }
  await start("Check unknown native error");
  fail({
    message: "New provider error",
    codexErrorInfo: "futureCode",
    additionalDetails: "Keep this diagnostic",
  });
  await page
    .locator(".native-error")
    .getByText("New provider error", { exact: true })
    .waitFor();
  await start("Check readable JSON failure");
  const wrappedError =
    'HTTP 502: {"error":{"message":"The service did not answer."}}';
  fail({ message: wrappedError, codexErrorInfo: "other" });
  await poll(
    async () =>
      (await page.locator(".native-error > span").innerText()) ===
      "The service did not answer.",
    "readable error message",
  );
  await page.locator(".native-error summary").click();
  assert.equal(
    JSON.parse(await page.locator(".native-error pre").innerText()).message,
    wrappedError,
  );
  await start("Check terminal precaution");
  proc.stdin.write(
    JSON.stringify({
      id: 992,
      method: "item/commandExecution/requestApproval",
      params: {
        threadId: agent.threadId,
        turnId: agent.turnId,
        command: ["echo", "fixture"],
        reason: "Approve the fixture command",
      },
    }) + "\n",
  );
  await page.getByRole("button", { name: "Approve", exact: true }).waitFor();
  await page.locator("#message").fill("Keep this unsent draft.");
  fail({
    message: "Native precaution",
    codexErrorInfo: "misalignmentPolicyViolation",
  });
  await page
    .locator(".native-error")
    .getByText("Chat stopped as a precaution", { exact: true })
    .waitFor();
  await poll(
    async () =>
      !!(await state()).runtime.agents.find((a) => a.id === agent.id)
        ?.nativeThreadBlock,
    "native thread precaution preserved",
  );
  assert.equal(await page.locator("#message").isDisabled(), true);
  assert.equal(
    await page.locator("#message").inputValue(),
    "Keep this unsent draft.",
  );
  assert.equal(await page.locator("#send").isDisabled(), true);
  await page
    .getByRole("button", { name: "Approve", exact: true })
    .waitFor({ state: "detached" });
  await page
    .getByText(
      "This chat stopped as a precaution. This request cannot resume it.",
      { exact: true },
    )
    .waitFor();
  await page.reload();
  await poll(
    async () =>
      !!(await state()).runtime.agents.find((a) => a.id === agent.id)
        ?.nativeThreadBlock,
    "native thread precaution preserved",
  );
  assert.equal(await page.locator("#message").isDisabled(), true);
  assert.equal(
    await page.locator("#message").inputValue(),
    "Keep this unsent draft.",
  );
  await page.setViewportSize({ width: 320, height: 740 });
  await page
    .locator(".native-error")
    .getByRole("button", { name: "Choose another chat", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Close conversations", exact: true })
    .waitFor();
  await page
    .getByRole("button", { name: "Close conversations", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Close conversations", exact: true })
    .waitFor({ state: "detached" });
  assert.equal(await page.evaluate(() => document.body.scrollWidth), 320);
  await page.screenshot({ path: join(root, "native-precaution-mobile.png") });
  const previousChats = new Set(
    (await state()).runtime.agents.map((a) => a.id),
  );
  await page
    .locator(".native-error")
    .getByRole("button", { name: "New chat", exact: true })
    .click();
  await poll(
    async () =>
      (await state()).runtime.agents
        .filter((a) => a.isLead)
        .some((a) => !previousChats.has(a.id) && a.empty),
    "new chat created",
  );
  await page.locator(".native-error").waitFor({ state: "detached" });
  await poll(
    () => page.locator("#message").isEnabled(),
    "new chat accepts a message after projection arrival",
  );
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ passed: true, evidence: root }));
} catch (error) {
  console.error(await page?.locator("body").innerText());
  throw error;
} finally {
  await browser?.close();
  proc.kill("SIGTERM");
}
