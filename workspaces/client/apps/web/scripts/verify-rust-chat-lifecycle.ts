#!/usr/bin/env node
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { SCALE_PROVIDER, requireScaleModel } from "./scaleVerifierHelpers.mjs";

const DEFAULT_MODEL = SCALE_PROVIDER.model;
const DEFAULT_EFFORT = SCALE_PROVIDER.effort;
const EXPECTED_REPLY = "STUDIO_RUST_LUNA_OK";
const ACTION_TIMEOUT_MS = 15_000;
const NAVIGATION_TIMEOUT_MS = 45_000;
const CREATE_TIMEOUT_MS = 90_000;
const MESSAGE_ACK_TIMEOUT_MS = 45_000;
const MESSAGE_TIMEOUT_MS = 120_000;
const CHAT_ROW_TIMEOUT_MS = 30_000;
const DELETE_ACK_TIMEOUT_MS = 30_000;
const DELETE_UI_TIMEOUT_MS = 20_000;
const POST_DELETE_OBSERVATION_MS = 4_000;
const EVIDENCE_CAPTURE_TIMEOUT_MS = 10_000;
const RECEIPT_TIMEOUT_MS = 10_000;
const CATALOG_TIMEOUT_MS = 10_000;
const TERMINAL_STATE_TIMEOUT_MS = 120_000;
const REVALIDATION_SETTLE_MS = 3_000;
const IDLE_OBSERVATION_MS = 15_000;
const POLL_INTERVAL_MS = 1_000;
const MAX_STEADY_REQUESTS_PER_ENDPOINT = 3;
const MAX_RECORDED_RESOURCES = 200;
const MAX_ANSWER_TEXT = 20_000;
// A durable input receipt is not proof that the provider has received the input.
const ACCEPTED_MESSAGE_RECEIPT_STATUSES = new Set([
  "pending",
  "queued",
  "accepted",
  "delivered",
  "sent",
]);

export function isAcceptedMessageReceiptStatus(status) {
  return ACCEPTED_MESSAGE_RECEIPT_STATUSES.has(status);
}

function argumentsFrom(argv) {
  const options = {};
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--resume") {
      options.resume = true;
      continue;
    }
    if (value === "--help" || value === "-h") {
      options.help = true;
      continue;
    }
    if (!value.startsWith("--"))
      throw new Error(`Unexpected argument: ${value}`);
    const key = value.slice(2);
    const next = argv[index + 1];
    if (!next || next.startsWith("--"))
      throw new Error(`Missing value for --${key}`);
    options[key] = next;
    index += 1;
  }
  return options;
}

function usage() {
  return [
    "Usage:",
    "  node scripts/verify-rust-chat-lifecycle.mjs --url HTTPS_URL --project NAME_OR_PATH --evidence-dir DIR",
    "  node scripts/verify-rust-chat-lifecycle.mjs --url HTTPS_URL --evidence-dir DIR --resume",
    "",
    "Options:",
    "  --profile-dir DIR   Persistent Chromium profile (defaults to EVIDENCE_DIR/profile)",
    `  --model ID          Lead model (defaults to ${DEFAULT_MODEL})`,
    "  --chromium PATH     Chromium executable (or set PLAYWRIGHT_CHROMIUM_EXECUTABLE)",
    "  --resume            Inspect prior journal/receipt and continue observation; never resend",
  ].join("\n");
}

function readJournal(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw new Error(`Cannot read journal ${path}: ${error.message}`);
  }
}

function saveJournal(path, journal) {
  journal.updatedAt = new Date().toISOString();
  const tempPath = `${path}.next`;
  writeFileSync(tempPath, `${JSON.stringify(journal, null, 2)}\n`, {
    encoding: "utf8",
    mode: 0o600,
  });
  renameSync(tempPath, path);
}

function elapsed(start) {
  return Math.round(performance.now() - start);
}

function ensureHttps(origin) {
  const url = new URL(origin);
  assert.equal(url.protocol, "https:", "The target URL must use HTTPS.");
  return url.origin;
}

function routePath(request) {
  try {
    return new URL(request.url()).pathname;
  } catch {
    return "";
  }
}

function chatSelector(id) {
  assert.match(
    id,
    /^[A-Za-z0-9_-]+$/,
    "Chat ID contains unsafe selector characters.",
  );
  return `[data-chat="${id}"]`;
}

export function selectedChatRowSelector(id) {
  const row = chatSelector(id);
  return `${row}[aria-current="true"], ${row}[aria-current="page"]`;
}

function redactUrl(url) {
  const parsed = new URL(url);
  for (const key of parsed.searchParams.keys())
    if (/token|secret|auth|credential/i.test(key))
      parsed.searchParams.set(key, "[redacted]");
  return parsed.href;
}

function redactValue(value) {
  if (Array.isArray(value)) return value.map(redactValue);
  if (!value || typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value).map(([key, item]) => [
      key,
      /token|secret|auth|credential|account.?key|api.?key/i.test(key)
        ? "[redacted]"
        : redactValue(item),
    ]),
  );
}

function apiRecord(response) {
  const request = response.request();
  return {
    at: new Date().toISOString(),
    observedMs: performance.now(),
    method: request.method(),
    url: redactUrl(response.url()),
    status: response.status(),
  };
}

function endpointCounts(requests) {
  return Object.fromEntries(
    requests.reduce((counts, request) => {
      const key = `${request.method} ${new URL(request.url).pathname}`;
      counts.set(key, (counts.get(key) || 0) + 1);
      return counts;
    }, new Map()),
  );
}

function failureCountsByStage(failures) {
  return Object.fromEntries(
    failures.reduce((counts, failure) => {
      counts.set(failure.stage, (counts.get(failure.stage) || 0) + 1);
      return counts;
    }, new Map()),
  );
}

function assertTrafficHealthy(traffic, journal) {
  const unexpectedFailures = traffic.failures;
  const nonResetHttpErrors = unexpectedFailures.filter(
    (item) => item.status !== 409,
  );
  if (
    nonResetHttpErrors.length ||
    unexpectedFailures.some((item) => !item.status)
  )
    throw new Error("An HTTP response error or request failure occurred.");
  const reset409s = unexpectedFailures.filter((item) => item.status === 409);
  const unrecovered409s = reset409s.filter((error) => {
    if (new URL(error.url).pathname !== "/api/sync/pull") return true;
    return !traffic.requests.some(
      (request) =>
        new URL(request.url).pathname === "/api/sync/pull" &&
        request.status < 400 &&
        (error.observedMs === undefined
          ? new Date(request.at).getTime() > new Date(error.at).getTime()
          : request.observedMs > error.observedMs),
    );
  });
  if (unrecovered409s.length)
    throw new Error(
      "A 409 did not recover through a later successful /api/sync/pull.",
    );
  const sustainedEndpoints = Object.entries(
    journal.stages.idle?.requestsByEndpoint || {},
  ).filter(([, count]) => count > MAX_STEADY_REQUESTS_PER_ENDPOINT);
  if (sustainedEndpoints.length)
    throw new Error(
      "Repeated endpoint traffic exceeded the steady interval revalidation budget.",
    );
  if (traffic.pageErrors.length)
    throw new Error("The browser reported an uncaught page error.");
  return reset409s;
}

async function jsonResponse(response) {
  try {
    return await response.json();
  } catch {
    return null;
  }
}

async function inspectReceipt(page, chatId, messageId) {
  const url = new URL("/api/messages/receipts", page.url());
  url.searchParams.set("agent", chatId);
  url.searchParams.set("ids", JSON.stringify([messageId]));
  const result = await page.evaluate(
    async ({ requestUrl, timeoutMs }) => {
      const response = await fetch(requestUrl, {
        credentials: "same-origin",
        signal: AbortSignal.timeout(timeoutMs),
      });
      let body;
      try {
        body = await response.json();
      } catch {
        body = null;
      }
      return { status: response.status, body };
    },
    { requestUrl: url.href, timeoutMs: RECEIPT_TIMEOUT_MS },
  );
  return result;
}

async function ownershipBaseline(page) {
  const existingChatIds = await page
    .locator("[data-chat]")
    .evaluateAll((rows) =>
      rows.map((row) => row.getAttribute("data-chat")).filter(Boolean),
    );
  const pendingCreationIds = await page.evaluate(() => {
    const ids = [];
    for (let index = 0; index < localStorage.length; index += 1) {
      const key = localStorage.key(index);
      if (!key?.startsWith("codex-pending-creation:")) continue;
      const raw = localStorage.getItem(key);
      if (!raw) continue;
      const request = JSON.parse(raw);
      if (typeof request?.id === "string" && request.id) ids.push(request.id);
    }
    return ids;
  });
  return {
    capturedAt: new Date().toISOString(),
    existingChatIds: [...new Set(existingChatIds)],
    pendingCreationIds: [...new Set(pendingCreationIds)],
  };
}

async function visibleAnswer(page) {
  const messages = page.locator(".message.assistant");
  const count = await messages.count();
  const visible = [];
  for (let index = 0; index < count; index += 1) {
    const item = messages.nth(index);
    if (!(await item.isVisible().catch(() => false))) continue;
    const text = (await item.innerText().catch(() => "")).trim();
    if (text) visible.push(text.slice(0, MAX_ANSWER_TEXT));
  }
  return visible.find((text) => text.includes(EXPECTED_REPLY)) || null;
}

async function promptAlreadyVisible(page) {
  const prompts = page.locator(".message.user");
  const count = await prompts.count();
  for (let index = 0; index < count; index += 1) {
    const text = await prompts
      .nth(index)
      .innerText()
      .catch(() => "");
    if (text.includes(`Reply with exactly ${EXPECTED_REPLY}`)) return true;
  }
  return false;
}

async function recordCreateNavigation(page, journal, journalPath) {
  const selectedRow = page.locator(selectedChatRowSelector(journal.chatId));
  await selectedRow.waitFor({ timeout: CHAT_ROW_TIMEOUT_MS });
  journal.stages.create.navigation ||= {
    signal: "selected-chat-row",
    previousAgentId: journal.stages.create.request?.previous || null,
    chatId: journal.chatId,
    observedAt: new Date().toISOString(),
  };
  saveJournal(journalPath, journal);
}

async function pageMetrics(page) {
  return page.evaluate((maxRecordedResources) => {
    const entries = performance.getEntriesByType("resource");
    const resources = entries.slice(-maxRecordedResources).map((entry) => ({
      name: new URL(entry.name, location.href).pathname,
      initiatorType: entry.initiatorType,
      durationMs: Math.round(entry.duration),
      transferSize: entry.transferSize || 0,
    }));
    const memory = performance.memory;
    return {
      resourceCount: resources.length,
      totalResourceCount: entries.length,
      recordedResourceTransferBytes: resources.reduce(
        (total, resource) => total + resource.transferSize,
        0,
      ),
      jsHeapUsedBytes: memory?.usedJSHeapSize ?? null,
      jsHeapLimitBytes: memory?.jsHeapSizeLimit ?? null,
      domNodeCount: document.getElementsByTagName("*").length,
      resources,
    };
  }, MAX_RECORDED_RESOURCES);
}

async function main() {
  const options = argumentsFrom(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(`${usage()}\n`);
    return;
  }
  assert.ok(options.url, "--url is required.");
  assert.ok(options["evidence-dir"], "--evidence-dir is required.");
  if (!options.resume)
    assert.ok(options.project, "--project is required for a new lifecycle.");
  if (!options.resume && options.model)
    assert.equal(
      options.model,
      SCALE_PROVIDER.model,
      "Only the approved cheap Claude model is allowed for a new lifecycle.",
    );

  const origin = ensureHttps(options.url);
  const evidenceDir = resolve(options["evidence-dir"]);
  const profileDir = resolve(
    options["profile-dir"] || join(evidenceDir, "profile"),
  );
  const journalPath = join(evidenceDir, "rust-chat-lifecycle.json");
  const journal = readJournal(journalPath) || {
    version: 1,
    origin,
    project: options.project || null,
    model: options.model || DEFAULT_MODEL,
    createdAt: new Date().toISOString(),
    stages: {},
    http: [],
    failures: [],
  };
  journal.stages ||= {};
  journal.http ||= [];
  journal.failures ||= [];
  assert.equal(
    journal.origin,
    origin,
    "Evidence belongs to another target URL.",
  );
  assert.equal(journal.version, 1, "Unsupported lifecycle journal version.");
  if (options.project && journal.project && journal.project !== options.project)
    throw new Error("The requested project differs from the saved journal.");
  if (options.resume && !readJournal(journalPath))
    throw new Error(
      "Resume requires an existing lifecycle journal proving ownership of the test chat.",
    );
  if (
    options.resume &&
    (!journal.chatId ||
      !journal.stages.create ||
      !["sent", "confirmed"].includes(journal.stages.create.state))
  )
    throw new Error(
      "Resume requires a journaled create request for the exact chat ID; an ID alone does not prove ownership.",
    );
  journal.expectedReply ||= EXPECTED_REPLY;
  journal.model ||= options.model || DEFAULT_MODEL;
  journal.project ||= options.project || null;
  if (
    !options.resume &&
    (journal.chatId || journal.stages.create || journal.stages.prompt)
  )
    throw new Error(
      "This evidence directory already has a lifecycle attempt; use --resume to inspect it.",
    );
  if (options.resume && journal.stages.delete?.state === "confirmed") {
    if (journal.result !== "passed" || !journal.summary)
      throw new Error(
        "Deletion is already confirmed but the journal is incomplete; inspect the evidence without repeating the lifecycle.",
      );
    process.stdout.write(`${JSON.stringify(journal.summary, null, 2)}\n`);
    return;
  }
  mkdirSync(evidenceDir, { recursive: true, mode: 0o700 });
  mkdirSync(profileDir, { recursive: true, mode: 0o700 });
  saveJournal(journalPath, journal);

  const webPackage = resolve(
    dirname(fileURLToPath(import.meta.url)),
    "../package.json",
  );
  const require = createRequire(webPackage);
  const { chromium } = require("playwright");
  const executablePath =
    options.chromium || process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE || undefined;
  const context = await chromium.launchPersistentContext(profileDir, {
    headless: true,
    ...(executablePath ? { executablePath } : {}),
    viewport: { width: 1440, height: 1000 },
    acceptDownloads: false,
  });
  const page = context.pages()[0] || (await context.newPage());
  page.setDefaultTimeout(ACTION_TIMEOUT_MS);
  page.setDefaultNavigationTimeout(NAVIGATION_TIMEOUT_MS);
  const traffic = {
    requests: [],
    allRequests: [],
    failures: [...journal.http, ...journal.failures],
    expectedStreamAborts: journal.expectedStreamAborts || [],
    pageErrors: journal.failure?.pageErrors || [],
    agentStates: [],
    stage: "startup",
  };
  page.on("pageerror", (error) => {
    traffic.pageErrors.push({
      at: new Date().toISOString(),
      stage: traffic.stage,
      message: error.message,
    });
  });
  page.on("request", (request) => {
    traffic.allRequests.push({
      at: new Date().toISOString(),
      method: request.method(),
      url: redactUrl(request.url()),
      stage: traffic.stage,
    });
  });
  page.on("response", async (response) => {
    if (
      !routePath(response.request()).startsWith("/api/") &&
      response.status() < 400
    )
      return;
    const record = { ...apiRecord(response), stage: traffic.stage };
    traffic.requests.push(record);
    if (response.status() >= 400) {
      let detail;
      try {
        const body = await response.json();
        if (typeof body.error === "string") detail = body.error.slice(0, 1500);
      } catch {}
      const failure = {
        ...record,
        detail,
        review409Separately: response.status() === 409,
      };
      traffic.failures.push(failure);
      journal.http.push(failure);
      saveJournal(journalPath, journal);
    }
    if (
      routePath(response.request()) === "/api/sync/pull" &&
      response.status() < 400
    ) {
      try {
        const body = await response.json();
        for (const document of body.documents || []) {
          if (document._deleted || typeof document.payload !== "string")
            continue;
          const entity = JSON.parse(document.payload);
          if (entity.collection !== "agent" || entity.id !== journal.chatId)
            continue;
          traffic.agentStates.push({
            at: new Date().toISOString(),
            seq: document.seq,
            status: entity.value?.status ?? null,
            inFlight: entity.value?.inFlight ?? null,
          });
        }
      } catch {}
    }
  });
  page.on("requestfailed", (request) => {
    const error = request.failure()?.errorText || "unknown request failure";
    if (
      routePath(request) === "/api/sync/stream" &&
      /ERR_ABORTED/i.test(error)
    ) {
      traffic.expectedStreamAborts.push({
        at: new Date().toISOString(),
        url: redactUrl(request.url()),
        error,
        stage: traffic.stage,
      });
      return;
    }
    const failure = {
      at: new Date().toISOString(),
      method: request.method(),
      url: redactUrl(request.url()),
      error,
      stage: traffic.stage,
    };
    traffic.failures.push(failure);
    journal.failures.push(failure);
    saveJournal(journalPath, journal);
  });

  await page.route("**/api/leads", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    const body = route.request().postDataJSON();
    assert.equal(
      body.account_key,
      SCALE_PROVIDER.accountKey,
      "Creation used another account.",
    );
    assert.equal(
      body.model,
      SCALE_PROVIDER.model,
      "Only the approved cheap Claude model may be invoked.",
    );
    assert.equal(
      body.fast_mode,
      SCALE_PROVIDER.fastMode,
      "Paid checks require fast mode off.",
    );
    assert.equal(
      body.model,
      journal.model,
      "Creation request used another model.",
    );
    assert.equal(
      body.effort,
      DEFAULT_EFFORT,
      "Creation request did not use the approved cheap Claude effort.",
    );
    assert.equal(
      body.cwd,
      journal.projectPath,
      `Creation request cwd must match the selected project (${journal.projectPath}).`,
    );
    assert.ok(body.id, "Creation request did not carry a stable chat ID.");
    const baseline = journal.ownershipBaseline;
    assert.ok(
      baseline,
      "No ownership baseline was saved before chat creation.",
    );
    if (
      baseline.existingChatIds.includes(body.id) ||
      baseline.pendingCreationIds.includes(body.id)
    )
      throw new Error(
        "Refused a create request whose identity existed before this lifecycle; it may be a pending user chat.",
      );
    if (journal.chatId && journal.chatId !== body.id)
      throw new Error(
        "Create request identity differs from the saved chat ID.",
      );
    journal.chatId = body.id;
    journal.stages.create = {
      state: "sent",
      request: redactValue(body),
      sentAt: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);
    await route.continue();
  });
  await page.route("**/api/messages", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    const body = route.request().postDataJSON();
    assert.equal(
      journal.model,
      SCALE_PROVIDER.model,
      "Refused a new prompt to a model outside the approved cheap Claude profile.",
    );
    if (body.room !== journal.chatId)
      throw new Error("Refused to send to a chat outside this lifecycle.");
    if (journal.messageId && journal.messageId !== body.id)
      throw new Error("Prompt identity differs from the saved message ID.");
    journal.messageId = body.id;
    journal.stages.prompt = {
      state: "sent",
      request: body,
      sentAt: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);
    await route.continue();
  });
  await page.route("**/api/conversation/delete", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    const body = route.request().postDataJSON();
    if (body.id !== journal.chatId)
      throw new Error("Refused to delete a chat outside this lifecycle.");
    if (
      !journal.stages.answer?.state ||
      journal.stages.answer.state !== "observed"
    )
      throw new Error(
        "Refused cleanup before observing the real model answer.",
      );
    journal.stages.delete = {
      state: "sent",
      request: body,
      sentAt: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);
    await route.continue();
  });

  const started = performance.now();
  try {
    await page.goto(origin, { waitUntil: "domcontentloaded" });
    await page.locator("body").waitFor();
    journal.stages.startup = {
      state: "loaded",
      elapsedMs: elapsed(started),
      at: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);

    if (options.resume) {
      assert.ok(
        journal.chatId,
        "Resume requires the chat ID saved in the lifecycle journal.",
      );
      traffic.stage = "resume-inspection";
      // Initial navigation already opened a fresh page. Reloading here aborts
      // its projection hydration and records failures caused by the verifier.
      if (journal.stages.delete?.state === "sent") {
        const stillVisible = await page
          .locator(chatSelector(journal.chatId))
          .count();
        if (!stillVisible)
          throw new Error(
            "A prior delete response was lost and the chat is absent from the reloaded UI; inspect authoritative deletion state before resuming.",
          );
      }
      if (journal.stages.delete?.state !== "confirmed")
        await page.locator(chatSelector(journal.chatId)).waitFor({
          timeout: CHAT_ROW_TIMEOUT_MS,
        });
      if (journal.stages.delete?.state !== "confirmed") {
        await page.locator(chatSelector(journal.chatId)).click();
        await page
          .getByRole("combobox", { name: "Message", exact: true })
          .waitFor();
      }
      if (journal.stages.create?.state === "sent") {
        journal.stages.create.state = "confirmed";
        journal.stages.create.confirmation =
          "chat row visible after reloading authoritative UI state";
        journal.stages.create.confirmedAt = new Date().toISOString();
        saveJournal(journalPath, journal);
      }
      if (journal.messageId) {
        const receipt = await inspectReceipt(
          page,
          journal.chatId,
          journal.messageId,
        );
        journal.stages.receiptInspection = {
          state: "observed",
          status: receipt.status,
          receipt: receipt.body,
          at: new Date().toISOString(),
        };
        saveJournal(journalPath, journal);
        if (receipt.status >= 400)
          throw new Error(
            `Receipt inspection failed with HTTP ${receipt.status}.`,
          );
        assert.equal(
          receipt.body?.agent,
          journal.chatId,
          "Receipt response belongs to another chat.",
        );
        assert.ok(
          Array.isArray(receipt.body?.items),
          "Receipt response has no items array.",
        );
        const receipts = receipt.body.items;
        assert.ok(
          receipts.every((item) => item.id === journal.messageId),
          "Receipt response included an unrequested message identity.",
        );
        const found = receipts.find((item) => item.id === journal.messageId);
        assert.ok(
          found,
          "No receipt confirms this exact message; the send outcome remains uncertain and will not be replayed.",
        );
        assert.ok(
          !found.agent || found.agent === journal.chatId,
          "Message receipt belongs to another chat.",
        );
        assert.ok(
          isAcceptedMessageReceiptStatus(found.status) ||
            ["reserved", "dispatching", "uncertain"].includes(found.status),
          `Message receipt has terminal nonaccepted status ${found.status}; the send will not be replayed.`,
        );
        journal.stages.prompt ||= {};
        journal.stages.prompt.state =
          found.status === "uncertain" ? "uncertain" : "accepted";
        journal.stages.prompt.receipt = found;
        journal.stages.prompt.sentAt ||= found.at
          ? new Date(found.at * 1000).toISOString()
          : undefined;
        saveJournal(journalPath, journal);
      }
    } else {
      traffic.stage = "create";
      const catalog = await page.evaluate(
        async ({ account, timeoutMs }) => {
          const response = await fetch(
            `/api/models?account_key=${encodeURIComponent(account)}`,
            {
              credentials: "same-origin",
              signal: AbortSignal.timeout(timeoutMs),
            },
          );
          if (!response.ok)
            throw new Error(`Model catalog returned HTTP ${response.status}.`);
          return response.json();
        },
        { account: SCALE_PROVIDER.accountKey, timeoutMs: CATALOG_TIMEOUT_MS },
      );
      requireScaleModel(catalog);
      const projectSections = page.locator("[data-project-path]");
      await projectSections.first().waitFor();
      const projectPath = await projectSections.evaluateAll(
        (sections, wanted) =>
          sections
            .find(
              (section) =>
                section.getAttribute("data-project-path") === wanted ||
                section
                  .querySelector(".project-tree-toggle > span[title]")
                  ?.getAttribute("title") === wanted ||
                section
                  .querySelector(".project-tree-toggle")
                  ?.getAttribute("title") === wanted,
            )
            ?.getAttribute("data-project-path") ?? null,
        options.project,
      );
      assert.ok(
        projectPath,
        `Project ${options.project} was not found in the UI or has no project path.`,
      );
      if (isAbsolute(options.project))
        assert.equal(
          projectPath,
          options.project,
          `Selected project path does not match ${options.project}.`,
        );
      const projectSection = page.locator(
        `[data-project-path=${JSON.stringify(projectPath)}]`,
      );
      assert.equal(
        await projectSection.count(),
        1,
        `Expected exactly one project row for ${projectPath}.`,
      );
      journal.projectPath = projectPath;
      const projectName = await projectSection
        .locator(".project-tree-toggle > span[title]")
        .getAttribute("title");
      assert.ok(projectName, "The project row has no accessible project name.");
      journal.ownershipBaseline = await ownershipBaseline(page);
      saveJournal(journalPath, journal);
      const projectHeading = projectSection.locator(".project-tree-heading");
      await projectHeading.waitFor();
      await projectHeading.hover();
      const newChatButton = projectSection
        .getByRole("button", {
          name: `New chat in ${projectName.trim()}`,
          exact: true,
        })
        .first();
      await newChatButton.click();
      const dialog = page.getByRole("dialog", {
        name: "New chat",
        exact: true,
      });
      await dialog.waitFor();
      const serverCards = dialog
        .getByRole("group", { name: "Chat server", exact: true })
        .getByRole("button");
      const cardCount = await serverCards.count();
      let matchingCard = -1;
      for (let index = 0; index < cardCount; index += 1) {
        const lines = (await serverCards.nth(index).innerText())
          .split("\n")
          .map((line) => line.trim());
        if (lines.includes(projectPath)) {
          matchingCard = index;
          if (
            (await serverCards.nth(index).getAttribute("aria-pressed")) ===
            "true"
          )
            break;
        }
      }
      assert.ok(
        matchingCard >= 0,
        `No available project server card points to the selected project path ${projectPath}.`,
      );
      if (
        (await serverCards.nth(matchingCard).getAttribute("aria-pressed")) !==
        "true"
      )
        await serverCards.nth(matchingCard).click();
      await dialog
        .getByRole("button", { name: "Main agent settings", exact: true })
        .click();
      const accountGroup = page.getByRole("group", {
        name: "Main agent account",
        exact: true,
      });
      await accountGroup
        .getByRole("radio", { name: "Claude", exact: true })
        .locator("..")
        .locator("label")
        .click();
      await accountGroup
        .locator(
          `[data-account-key=${JSON.stringify(SCALE_PROVIDER.accountKey)}]`,
        )
        .click();
      const modelOption = page.locator(
        `[role="option"][data-value=${JSON.stringify(journal.model)}]`,
      );
      await modelOption.waitFor();
      const value = await modelOption.getAttribute("data-value");
      if (value !== journal.model)
        throw new Error(
          `The visible model option was ${value}; expected ${journal.model}.`,
        );
      await modelOption.click();
      await page.getByText("Low", { exact: true }).click();
      await dialog
        .getByRole("button", { name: "Main agent settings", exact: true })
        .click();

      const createStarted = performance.now();
      const createResponse = page.waitForResponse(
        (response) =>
          response.request().method() === "POST" &&
          routePath(response.request()) === "/api/leads",
        { timeout: CREATE_TIMEOUT_MS },
      );
      await dialog
        .getByRole("button", { name: "Start chat", exact: true })
        .click();
      const response = await createResponse;
      const body = await jsonResponse(response);
      assert.equal(
        response.status(),
        200,
        `Chat creation returned HTTP ${response.status()}.`,
      );
      assert.equal(
        body?.id,
        journal.chatId,
        "Created chat identity did not match the journal.",
      );
      journal.stages.create = {
        ...journal.stages.create,
        state: "confirmed",
        response: body,
        elapsedMs: elapsed(createStarted),
        confirmedAt: new Date().toISOString(),
      };
      saveJournal(journalPath, journal);
      await page.locator(chatSelector(journal.chatId)).waitFor({
        timeout: CHAT_ROW_TIMEOUT_MS,
      });
      await page
        .getByRole("combobox", { name: "Message", exact: true })
        .waitFor();
      await recordCreateNavigation(page, journal, journalPath);
    }

    if ((!options.resume || !journal.messageId) && !journal.stages.prompt) {
      if (options.resume && (await promptAlreadyVisible(page)))
        throw new Error(
          "The prompt is visible in the transcript without a saved request identity; refusing to send it again.",
        );
      traffic.stage = "prompt";
      const composer = page.getByRole("combobox", {
        name: "Message",
        exact: true,
      });
      await composer.fill(
        `Reply with exactly ${EXPECTED_REPLY}. Do not use tools or edit files.`,
      );
      const promptStarted = performance.now();
      const messageResponse = page.waitForResponse(
        (response) =>
          response.request().method() === "POST" &&
          routePath(response.request()) === "/api/messages",
        { timeout: MESSAGE_ACK_TIMEOUT_MS },
      );
      await page
        .getByRole("button", { name: "Send message", exact: true })
        .click();
      const response = await messageResponse;
      const body = await jsonResponse(response);
      assert.equal(
        response.status(),
        200,
        `Message send returned HTTP ${response.status()}.`,
      );
      assert.equal(
        body?.id,
        journal.messageId,
        "Message receipt did not confirm the exact submitted ID.",
      );
      assert.ok(
        !body.room || body.room === journal.chatId,
        "Message receipt belongs to another room.",
      );
      assert.ok(
        !body.agent || body.agent === journal.chatId,
        "Message receipt belongs to another chat.",
      );
      assert.ok(
        isAcceptedMessageReceiptStatus(body.status),
        `Message receipt did not confirm acceptance (status ${body.status}).`,
      );
      journal.stages.prompt = {
        ...journal.stages.prompt,
        state: "accepted",
        receipt: body,
        ackElapsedMs: elapsed(promptStarted),
        acknowledgedAt: new Date().toISOString(),
      };
      saveJournal(journalPath, journal);
    }

    traffic.stage = "answer";
    const answerStarted = performance.now();
    let answer =
      journal.stages.answer?.state === "observed"
        ? journal.stages.answer.text
        : await visibleAnswer(page);
    while (!answer && elapsed(answerStarted) < MESSAGE_TIMEOUT_MS) {
      await page.waitForTimeout(POLL_INTERVAL_MS);
      answer = await visibleAnswer(page);
    }
    assert.ok(
      answer,
      `No real assistant response containing ${EXPECTED_REPLY} appeared.`,
    );
    assert.equal(
      answer.trim(),
      EXPECTED_REPLY,
      "Assistant response did not match the exact requested text.",
    );
    journal.stages.answer = {
      state: "observed",
      text: answer,
      elapsedMs: elapsed(answerStarted),
      observedAt: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);

    traffic.stage = "terminal-state";
    const terminalStarted = performance.now();
    const promptSentAt =
      journal.stages.prompt?.sentAt || journal.stages.prompt?.acknowledgedAt;
    assert.ok(
      promptSentAt,
      "The prompt journal has no timestamp for post-prompt agent-state verification.",
    );
    const latestAgentState = () =>
      [...traffic.agentStates].sort((left, right) => right.seq - left.seq)[0];
    let terminalState = latestAgentState();
    while (
      (!terminalState ||
        !["idle", "completed"].includes(terminalState.status) ||
        terminalState.inFlight !== false ||
        terminalState.at < promptSentAt) &&
      elapsed(terminalStarted) < TERMINAL_STATE_TIMEOUT_MS
    ) {
      await page.waitForTimeout(POLL_INTERVAL_MS);
      terminalState = latestAgentState();
    }
    assert.ok(
      terminalState &&
        ["idle", "completed"].includes(terminalState.status) &&
        terminalState.inFlight === false &&
        terminalState.at >= promptSentAt,
      "No post-prompt sync state confirmed the agent idle or completed with inFlight=false.",
    );
    journal.stages.terminal = {
      state: "confirmed",
      ...terminalState,
      observedAt: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);

    traffic.stage = "revalidation-settle";
    const settleStart = traffic.allRequests.length;
    await page.waitForTimeout(REVALIDATION_SETTLE_MS);
    const settleRequests = traffic.allRequests.slice(settleStart);

    traffic.stage = "steady-idle";
    const requestsBeforeIdle = traffic.allRequests.length;
    const failuresBeforeIdle = traffic.failures.length;
    const idleStarted = performance.now();
    await page.waitForTimeout(IDLE_OBSERVATION_MS);
    const idleRequests = traffic.allRequests.slice(requestsBeforeIdle);
    const idleFailures = traffic.failures.slice(failuresBeforeIdle);
    journal.stages.idle = {
      durationMs: elapsed(idleStarted),
      revalidationSettleMs: REVALIDATION_SETTLE_MS,
      revalidationSettleRequestCount: settleRequests.length,
      revalidationSettleRequestsByEndpoint: endpointCounts(settleRequests),
      requestCount: idleRequests.length,
      requests: idleRequests,
      requestsByEndpoint: endpointCounts(idleRequests),
      failures: idleFailures,
      http409Count: idleFailures.filter((item) => item.status === 409).length,
      resources: await pageMetrics(page),
      observedAt: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);
    if (journal.stages.delete?.state === "sent")
      throw new Error(
        "A prior delete response was lost and the chat still exists; inspect manually before any retry.",
      );
    if (journal.stages.delete?.state !== "confirmed") {
      traffic.stage = "delete";
      const row = page.locator(`[data-sidebar-item="${journal.chatId}"]`);
      await row.waitFor();
      await row.getByRole("button", { name: /^Actions for / }).click();
      await page.getByRole("menuitem", { name: "Delete", exact: true }).click();
      const confirmation = page.getByRole("dialog", {
        name: "Delete chat?",
        exact: true,
      });
      await confirmation.waitFor();
      const deletionStarted = performance.now();
      const deleteResponse = page.waitForResponse(
        (response) =>
          response.request().method() === "POST" &&
          routePath(response.request()) === "/api/conversation/delete",
        { timeout: DELETE_ACK_TIMEOUT_MS },
      );
      await confirmation
        .getByRole("button", { name: "Delete chat", exact: true })
        .click();
      const response = await deleteResponse;
      const body = await jsonResponse(response);
      assert.equal(
        response.status(),
        200,
        `Chat deletion returned HTTP ${response.status()}.`,
      );
      const deleted =
        typeof body?.deleted === "string"
          ? [body.deleted]
          : body?.deleted || [];
      assert.ok(
        deleted.includes(journal.chatId),
        "Delete response did not confirm this chat ID.",
      );
      await page.locator(chatSelector(journal.chatId)).waitFor({
        state: "detached",
        timeout: DELETE_UI_TIMEOUT_MS,
      });
      journal.stages.delete = {
        ...journal.stages.delete,
        state: "confirmed",
        response: body,
        elapsedMs: elapsed(deletionStarted),
        confirmedAt: new Date().toISOString(),
      };
      saveJournal(journalPath, journal);
    }

    traffic.stage = "post-delete-observation";
    const postDeleteRequestStart = traffic.allRequests.length;
    const postDeleteFailureStart = traffic.failures.length;
    const postDeleteStarted = performance.now();
    await page.waitForTimeout(POST_DELETE_OBSERVATION_MS);
    const postDeleteRequests = traffic.allRequests.slice(
      postDeleteRequestStart,
    );
    const postDeleteFailures = traffic.failures.slice(postDeleteFailureStart);
    journal.stages.delete.quietObservation = {
      durationMs: elapsed(postDeleteStarted),
      requestCount: postDeleteRequests.length,
      requestsByEndpoint: endpointCounts(postDeleteRequests),
      failures: postDeleteFailures,
      observedAt: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);

    // Retire the owned, completed chat before reporting recorded traffic
    // failures. The final assertion still rejects the original failure.
    const finalReset409s = assertTrafficHealthy(traffic, journal);
    journal.result = "passed";
    journal.completedAt = new Date().toISOString();
    journal.summary = {
      chatId: journal.chatId,
      messageId: journal.messageId,
      model: journal.model,
      createLatencyMs: journal.stages.create?.elapsedMs ?? null,
      messageAckLatencyMs: journal.stages.prompt?.ackElapsedMs ?? null,
      answerLatencyMs: journal.stages.answer?.elapsedMs ?? null,
      deleteLatencyMs: journal.stages.delete?.elapsedMs ?? null,
      postDeleteObservationMs:
        journal.stages.delete?.quietObservation?.durationMs ?? null,
      postDeleteRequestsByEndpoint:
        journal.stages.delete?.quietObservation?.requestsByEndpoint ?? {},
      idleDurationMs: journal.stages.idle?.durationMs ?? null,
      idleRequestCount: journal.stages.idle?.requestCount ?? null,
      idleRequestsByEndpoint: journal.stages.idle?.requestsByEndpoint ?? {},
      terminalAgentState: journal.stages.terminal ?? null,
      idleHttpErrors: journal.stages.idle?.failures ?? [],
      recoveredSyncReset409Count: finalReset409s.length,
      startupHttp409Count: traffic.failures.filter(
        (item) => item.stage === "startup" && item.status === 409,
      ).length,
      steadyIdleHttp409Count: journal.stages.idle?.http409Count ?? 0,
      httpFailuresByStage: failureCountsByStage(traffic.failures),
      httpFailures: traffic.failures,
      pageErrors: traffic.pageErrors,
      expectedStreamAborts: traffic.expectedStreamAborts,
    };
    journal.expectedStreamAborts = traffic.expectedStreamAborts;
    saveJournal(journalPath, journal);
    process.stdout.write(`${JSON.stringify(journal.summary, null, 2)}\n`);
  } catch (error) {
    journal.result = "incomplete";
    journal.failure = {
      at: new Date().toISOString(),
      stage: traffic.stage,
      message: String(error?.stack || error),
      requestCount: traffic.requests.length,
      failures: traffic.failures,
      pageErrors: traffic.pageErrors,
      expectedStreamAborts: traffic.expectedStreamAborts,
      screenshot: join(evidenceDir, "rust-chat-lifecycle-failure.png"),
    };
    try {
      await page.screenshot({
        path: journal.failure.screenshot,
        fullPage: true,
        timeout: EVIDENCE_CAPTURE_TIMEOUT_MS,
      });
    } catch {}
    saveJournal(journalPath, journal);
    throw error;
  } finally {
    await context.close();
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] || "").href) {
  main().catch((error) => {
    process.stderr.write(`${error?.stack || error}\n`);
    process.exitCode = 1;
  });
}
