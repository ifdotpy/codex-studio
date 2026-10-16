#!/usr/bin/env node
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const LEAD_PROVIDER = "claude";
const WORKER_MODEL = "gpt-6-luna";
const WORKER_EFFORT = "high";
const ACTION_TIMEOUT_MS = 15_000;
const NAVIGATION_TIMEOUT_MS = 45_000;
const READBACK_TIMEOUT_MS = 30_000;

function parseArgs(argv) {
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
    "  node scripts/verify-rust-worker-defaults.mjs --url HTTPS_URL --chat-id CHAT_UUID --lead-account ACCOUNT_KEY --evidence-dir DIR --chromium PATH",
    "  node scripts/verify-rust-worker-defaults.mjs --url HTTPS_URL --chat-id CHAT_UUID --lead-account ACCOUNT_KEY --evidence-dir DIR --resume --chromium PATH",
    "",
    "A new run saves worker defaults through the UI once. Resume is readback-only and never repeats a POST.",
    "--profile-dir DIR defaults to EVIDENCE_DIR/profile; PLAYWRIGHT_CHROMIUM_EXECUTABLE is also accepted.",
  ].join("\n");
}

function readJournal(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw new Error(`Cannot read evidence journal: ${error.message}`);
  }
}

function saveJournal(path, journal) {
  journal.updatedAt = new Date().toISOString();
  const temporary = `${path}.next`;
  writeFileSync(temporary, `${JSON.stringify(journal, null, 2)}\n`, {
    encoding: "utf8",
    mode: 0o600,
  });
  renameSync(temporary, path);
}

function ensureHttps(value) {
  const url = new URL(value);
  assert.equal(url.protocol, "https:", "The target URL must use HTTPS.");
  assert.equal(
    url.username,
    "",
    "The target URL must not include credentials.",
  );
  assert.equal(
    url.password,
    "",
    "The target URL must not include credentials.",
  );
  assert.equal(url.pathname, "/", "The target URL must be an origin only.");
  assert.equal(url.search, "", "The target URL must not include a query.");
  assert.equal(url.hash, "", "The target URL must not include a fragment.");
  return url.origin;
}

function localClassicViewUrl(origin) {
  const url = new URL(origin);
  url.searchParams.set("studio-server", "local");
  url.searchParams.set("studio-navigation", "classic");
  return url.toString();
}

async function assertLocalClassicView(page, origin) {
  const pageUrl = new URL(page.url());
  assert.equal(pageUrl.origin, origin, "The target page changed origin.");
  assert.equal(
    pageUrl.searchParams.get("studio-server"),
    "local",
    "The target page is not the local server view.",
  );
  assert.equal(
    pageUrl.searchParams.get("studio-navigation"),
    "classic",
    "The target page is not using classic navigation.",
  );
  assert.equal(
    await page.locator("html").getAttribute("data-server-view"),
    "true",
    "The local server view did not mount.",
  );
  assert.equal(
    await page.locator("html").getAttribute("data-server-navigation"),
    "classic",
    "Classic navigation did not mount.",
  );
}

function safeChatSelector(chatId) {
  assert.match(chatId, /^[A-Za-z0-9_-]+$/, "Unsafe chat ID for UI selector.");
  return `[data-chat=${JSON.stringify(chatId)}]`;
}

function agentSnapshot(document) {
  if (
    document?.collection !== "agent" ||
    document._deleted ||
    !document.value ||
    typeof document.value !== "object"
  )
    return null;
  return {
    seq: document.seq,
    agent: document.value,
  };
}

async function readServerIdentity(page, expectedOrigin) {
  const result = await page.evaluate(
    async ({ origin, timeoutMs }) => {
      if (location.origin !== origin)
        throw new Error("The page origin changed before server readback.");
      const response = await fetch(new URL("/api/multi-server", origin), {
        credentials: "same-origin",
        cache: "no-store",
        signal: AbortSignal.timeout(timeoutMs),
      });
      let body = null;
      try {
        body = await response.json();
      } catch {}
      return {
        origin: location.origin,
        status: response.status,
        serverId: body?.identity?.serverId,
      };
    },
    { origin: expectedOrigin, timeoutMs: READBACK_TIMEOUT_MS },
  );
  assert.equal(
    result.origin,
    expectedOrigin,
    "Server read used another origin.",
  );
  assert.equal(result.status, 200, "Could not read the local server identity.");
  assert.ok(
    typeof result.serverId === "string" && result.serverId,
    "Local server identity has no server ID.",
  );
  return result.serverId;
}

async function readAuthoritativeLead(page, chatId, expectedOrigin, serverId) {
  const result = await page.evaluate(
    async ({ agentId, origin, expectedServerId, timeoutMs }) => {
      if (location.origin !== origin)
        throw new Error("The page origin changed before lead readback.");
      const identityResponse = await fetch(
        new URL("/api/multi-server", origin),
        {
          credentials: "same-origin",
          cache: "no-store",
          signal: AbortSignal.timeout(timeoutMs),
        },
      );
      let identity = null;
      try {
        identity = await identityResponse.json();
      } catch {}
      if (
        identityResponse.status !== 200 ||
        identity?.identity?.serverId !== expectedServerId
      )
        throw new Error("Server identity changed before lead readback.");
      const url = new URL("/api/sync/pull", origin);
      url.searchParams.set("after", "0");
      url.searchParams.set("fresh", "1");
      url.searchParams.set("reset", "1");
      url.searchParams.set("priorityId", agentId);
      const response = await fetch(url, {
        credentials: "same-origin",
        cache: "no-store",
        signal: AbortSignal.timeout(timeoutMs),
      });
      let body = null;
      try {
        body = await response.json();
      } catch {}
      return {
        origin: url.origin,
        serverId: identity.identity.serverId,
        status: response.status,
        documents: body?.documents || [],
      };
    },
    {
      agentId: chatId,
      origin: expectedOrigin,
      expectedServerId: serverId,
      timeoutMs: READBACK_TIMEOUT_MS,
    },
  );
  assert.equal(result.origin, expectedOrigin, "Lead read used another origin.");
  assert.equal(result.serverId, serverId, "Lead read used another server.");
  assert.equal(
    result.status,
    200,
    `Authoritative lead read returned HTTP ${result.status}.`,
  );
  const document = result.documents.find((item) => {
    if (item.id === chatId || item.id?.endsWith(`:${chatId}`)) return true;
    if (typeof item.payload !== "string") return false;
    try {
      const entity = JSON.parse(item.payload);
      return entity?.collection === "agent" && entity.id === chatId;
    } catch {
      return false;
    }
  });
  assert.ok(document, "Authoritative read did not include the target lead.");
  assert.equal(document._deleted, false, "The target lead is deleted.");
  assert.equal(typeof document.payload, "string", "Lead read has no payload.");
  return agentSnapshot({
    ...JSON.parse(document.payload),
    seq: document.seq,
  });
}

function validateLead(snapshot, chatId, leadAccountKey, expected = null) {
  const agent = snapshot?.agent;
  assert.equal(agent?.id, chatId, "Sync snapshot belongs to another chat.");
  assert.equal(agent?.isLead, true, "The selected chat is not its lead agent.");
  assert.equal(agent?.accountKey, leadAccountKey, "Lead account changed.");
  assert.equal(agent?.provider, LEAD_PROVIDER, "Lead provider changed.");
  assert.ok(
    typeof agent.model === "string" && agent.model,
    "Lead model is missing.",
  );
  if (expected) {
    assert.equal(
      agent.accountKey,
      expected.accountKey,
      "Lead account changed during verification.",
    );
    assert.equal(
      agent.provider,
      expected.provider,
      "Lead provider changed during verification.",
    );
    assert.equal(
      agent.model,
      expected.model,
      "Lead model changed during verification.",
    );
  }
  return {
    id: agent.id,
    accountKey: agent.accountKey,
    provider: agent.provider,
    model: agent.model,
    workerDefaults: agent.workerDefaults || null,
  };
}

function assertDesiredDefaults(workerDefaults) {
  assert.ok(
    workerDefaults && typeof workerDefaults === "object",
    "Worker defaults are missing from the authoritative snapshot.",
  );
  assert.equal(
    workerDefaults.accountKey ?? null,
    null,
    "Worker account is pinned; expected Automatic.",
  );
  assert.equal(
    workerDefaults.model,
    WORKER_MODEL,
    "Worker model does not match Luna.",
  );
  assert.equal(
    workerDefaults.effort,
    WORKER_EFFORT,
    "Worker reasoning does not match High.",
  );
  assert.equal(workerDefaults.fastMode, false, "Unexpected Fast mode change.");
  assert.equal(
    workerDefaults.daybreakEnabled,
    false,
    "Unexpected Daybreak change.",
  );
  assert.equal(
    workerDefaults.delegationDepth,
    1,
    "Unexpected delegation depth change.",
  );
}

function assertWritePreflight(workerDefaults) {
  assert.ok(
    workerDefaults && typeof workerDefaults === "object",
    "Worker defaults are missing from the authoritative snapshot.",
  );
  assert.equal(
    workerDefaults.accountKey ?? null,
    null,
    "Worker account is already pinned; verifier will not change it.",
  );
  assert.equal(
    workerDefaults.effort,
    WORKER_EFFORT,
    "Worker reasoning is not already High; verifier will not make an extra settings write.",
  );
  assert.equal(
    workerDefaults.fastMode,
    false,
    "Fast mode is enabled; verifier will not change it.",
  );
  assert.equal(
    workerDefaults.daybreakEnabled,
    false,
    "Daybreak is enabled; verifier will not change it.",
  );
  assert.equal(
    workerDefaults.delegationDepth,
    1,
    "Delegation depth is not 1; verifier will not change it.",
  );
}

function expectedRequest(body, baseline, chatId) {
  assert.deepEqual(
    Object.keys(body).sort(),
    ["expected_account_key", "id", "worker_defaults"],
    "Settings request has unexpected top-level fields.",
  );
  assert.equal(body?.id, chatId, "Settings request targets another chat.");
  assert.equal(
    body?.expected_account_key,
    baseline.accountKey,
    "Settings request lead guard changed.",
  );
  const defaults = body?.worker_defaults;
  assert.ok(
    defaults && typeof defaults === "object",
    "Settings request has no worker_defaults.",
  );
  assert.deepEqual(
    Object.keys(defaults).sort(),
    [
      "account_key",
      "daybreak_enabled",
      "delegation_depth",
      "effort",
      "fast_mode",
      "model",
    ],
    "Settings request has unexpected worker-default fields.",
  );
  assert.equal(
    defaults.account_key,
    null,
    "Settings request pins a worker account.",
  );
  assert.equal(
    defaults.model,
    WORKER_MODEL,
    "Settings request model is not Luna.",
  );
  assert.equal(
    defaults.effort,
    WORKER_EFFORT,
    "Settings request reasoning is not High.",
  );
  assert.equal(
    defaults.fast_mode,
    false,
    "Settings request changes Fast mode.",
  );
  assert.equal(
    defaults.daybreak_enabled,
    false,
    "Settings request changes Daybreak.",
  );
  assert.equal(
    defaults.delegation_depth,
    1,
    "Settings request changes delegation depth.",
  );
  return body;
}

function matchesSettingsResponse(response, origin, chatId) {
  try {
    const request = response.request();
    const url = new URL(response.url());
    return (
      request.method() === "POST" &&
      url.origin === origin &&
      url.pathname === "/api/conversation" &&
      request.postDataJSON()?.id === chatId
    );
  } catch {
    return false;
  }
}

function responseSummary(body) {
  return {
    id: body?.id ?? null,
    accountKey: body?.accountKey ?? null,
    provider: body?.provider ?? null,
    model: body?.model ?? null,
    workerDefaults: body?.workerDefaults ?? null,
  };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(`${usage()}\n`);
    return;
  }
  assert.ok(options.url, "--url is required.");
  assert.ok(options["chat-id"], "--chat-id is required.");
  assert.ok(options["lead-account"], "--lead-account is required.");
  const chatId = options["chat-id"];
  const leadAccountKey = options["lead-account"];
  assert.match(
    chatId,
    /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i,
    "--chat-id must be a UUID.",
  );
  assert.match(
    leadAccountKey,
    /^[A-Za-z0-9._-]+$/,
    "--lead-account contains unsupported characters.",
  );
  assert.ok(options["evidence-dir"], "--evidence-dir is required.");

  const origin = ensureHttps(options.url);
  const evidenceDir = resolve(options["evidence-dir"]);
  const profileDir = resolve(
    options["profile-dir"] || join(evidenceDir, "profile"),
  );
  const journalPath = join(evidenceDir, `rust-worker-defaults-${chatId}.json`);
  const existing = readJournal(journalPath);
  if (options.resume)
    assert.ok(existing, "Resume requires the existing evidence journal.");
  else
    assert.equal(
      existing,
      null,
      "Evidence already exists; use --resume, which never writes.",
    );
  const journal = existing || {
    version: 1,
    origin,
    chatId,
    leadAccountKey,
    createdAt: new Date().toISOString(),
    write: { state: "not-sent" },
  };
  assert.equal(journal.version, 1, "Unsupported evidence journal version.");
  assert.equal(journal.origin, origin, "Evidence belongs to another server.");
  assert.equal(journal.chatId, chatId, "Evidence belongs to another chat.");
  assert.equal(
    journal.leadAccountKey,
    leadAccountKey,
    "Evidence belongs to another lead account.",
  );
  if (options.resume && journal.write?.state === "not-sent")
    throw new Error(
      "No write was sent in this journal; start with a new evidence directory.",
    );

  mkdirSync(evidenceDir, { recursive: true, mode: 0o700 });
  mkdirSync(profileDir, { recursive: true, mode: 0o700 });
  const webPackage = resolve(
    dirname(fileURLToPath(import.meta.url)),
    "../package.json",
  );
  const { chromium } = createRequire(webPackage)("playwright");
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

  let postedBody = null;
  let blockedWrite = null;

  await page.route("**/api/conversation", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    if (options.resume) {
      blockedWrite = "Resume mode blocks all settings writes.";
      await route.abort();
      return;
    }
    try {
      assert.equal(
        new URL(route.request().url()).origin,
        origin,
        "Settings update must target the requested server origin.",
      );
      const body = expectedRequest(
        route.request().postDataJSON(),
        journal.baseline,
        chatId,
      );
      if (journal.write.state !== "not-sent")
        throw new Error("A settings write was already attempted.");
      postedBody = body;
      journal.write = {
        state: "sent",
        request: body,
        sentAt: new Date().toISOString(),
      };
      saveJournal(journalPath, journal);
      await route.continue();
    } catch (error) {
      blockedWrite = error.message;
      journal.blockedAttempts ||= [];
      journal.blockedAttempts.push({
        operation: "settings",
        origin: new URL(route.request().url()).origin,
        path: new URL(route.request().url()).pathname,
        reason: error.message,
        at: new Date().toISOString(),
      });
      saveJournal(journalPath, journal);
      await route.fulfill({
        status: 409,
        contentType: "application/json",
        body: JSON.stringify({
          error: "Verifier blocked an unexpected settings write.",
        }),
      });
    }
  });
  for (const path of [
    "/api/messages",
    "/api/leads",
    "/api/conversation/delete",
  ]) {
    await page.route(`**${path}`, async (route) => {
      if (route.request().method() !== "POST") return route.continue();
      await route.abort();
      journal.blockedOperations ||= [];
      journal.blockedOperations.push({ path, at: new Date().toISOString() });
      saveJournal(journalPath, journal);
    });
  }

  try {
    await page.goto(localClassicViewUrl(origin), {
      waitUntil: "domcontentloaded",
    });
    await assertLocalClassicView(page, origin);
    const serverId = await readServerIdentity(page, origin);
    if (journal.serverId)
      assert.equal(
        journal.serverId,
        serverId,
        "Evidence belongs to another local server identity.",
      );
    else {
      journal.serverId = serverId;
    }
    const snapshot = await readAuthoritativeLead(
      page,
      chatId,
      origin,
      serverId,
    );
    if (!existing?.serverId) saveJournal(journalPath, journal);
    const lead = validateLead(
      snapshot,
      chatId,
      leadAccountKey,
      journal.baseline || null,
    );

    if (!journal.baseline) {
      assertWritePreflight(lead.workerDefaults);
      journal.baseline = { ...lead, seq: snapshot.seq };
      journal.write = { state: "not-sent" };
      journal.preflight =
        "Target lead identity and intended worker defaults confirmed before UI write.";
      saveJournal(journalPath, journal);
    }
    const chat = page.locator(safeChatSelector(chatId));
    await chat.waitFor();

    if (options.resume) {
      const readback = snapshot;
      const currentLead = validateLead(
        readback,
        chatId,
        leadAccountKey,
        journal.baseline,
      );
      const matches = (() => {
        try {
          assertDesiredDefaults(currentLead.workerDefaults);
          return true;
        } catch {
          return false;
        }
      })();
      journal.readback = {
        state: matches
          ? "matches-requested-values"
          : "does-not-match-requested-values",
        seq: readback?.seq ?? null,
        lead: currentLead,
        at: new Date().toISOString(),
        writeOutcome:
          journal.write.state === "confirmed"
            ? "response-confirmed"
            : "still-uncertain; no retry sent",
      };
      saveJournal(journalPath, journal);
      process.stdout.write(
        `${JSON.stringify({ state: journal.readback.state, writeOutcome: journal.readback.writeOutcome, chatId, lead: { accountKey: currentLead.accountKey, provider: currentLead.provider, model: currentLead.model }, workerDefaults: currentLead.workerDefaults }, null, 2)}\n`,
      );
      return;
    }

    assert.equal(
      journal.write.state,
      "not-sent",
      "A write attempt already exists; resume is readback-only.",
    );
    await chat.click();
    await page
      .getByRole("button", { name: "Chat settings", exact: true })
      .click();
    const settingsDialog = page.getByRole("dialog", {
      name: "Chat settings",
      exact: true,
    });
    await settingsDialog
      .getByRole("button", { name: "Subagent defaults", exact: true })
      .click();
    await page.getByRole("button", { name: "Worker", exact: true }).click();
    const automatic = page
      .getByRole("group", { name: "Subagent account", exact: true })
      .getByRole("button", { name: "Automatic", exact: true });
    assert.equal(
      await automatic.getAttribute("aria-pressed"),
      "true",
      "Worker account is not Automatic.",
    );
    assert.equal(
      await page
        .getByLabel("Default subagent reasoning", { exact: true })
        .getAttribute("data-value"),
      WORKER_EFFORT,
      "Worker reasoning is not High.",
    );
    const codexProvider = page.getByLabel("Subagent account provider", {
      exact: true,
    });
    await codexProvider.locator("label").filter({ hasText: "Codex" }).click();
    assert.equal(
      await automatic.getAttribute("aria-pressed"),
      "true",
      "Changing model provider pinned a worker account.",
    );
    const exactLuna = page.locator(
      `[role="option"][data-value="${WORKER_MODEL}"]`,
    );
    await exactLuna.waitFor();
    const responsePromise = page.waitForResponse(
      (response) => matchesSettingsResponse(response, origin, chatId),
      { timeout: READBACK_TIMEOUT_MS },
    );
    await exactLuna.click();
    const response = await responsePromise;
    assert.equal(blockedWrite, null, blockedWrite || undefined);
    assert.ok(postedBody, "No exact worker defaults request was recorded.");
    const responseBody = await response.json().catch(() => null);
    journal.write = {
      state: response.status() === 200 ? "response-received" : "response-error",
      status: response.status(),
      request: postedBody,
      response: responseSummary(responseBody),
      responseAt: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);
    assert.equal(
      response.status(),
      200,
      `Settings save returned HTTP ${response.status()}; do not retry this write.`,
    );
    const canonical = validateLead(
      { agent: responseBody, seq: null },
      chatId,
      leadAccountKey,
      journal.baseline,
    );
    assertDesiredDefaults(canonical.workerDefaults);
    journal.write.state = "confirmed";
    journal.write.canonical = canonical;
    saveJournal(journalPath, journal);

    const refreshed = await readAuthoritativeLead(
      page,
      chatId,
      origin,
      serverId,
    );
    const readbackLead = validateLead(
      refreshed,
      chatId,
      leadAccountKey,
      journal.baseline,
    );
    assertDesiredDefaults(readbackLead.workerDefaults);
    journal.readback = {
      state: "confirmed",
      seq: refreshed.seq,
      lead: readbackLead,
      at: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);
    process.stdout.write(
      `${JSON.stringify({ state: "passed", chatId, lead: { accountKey: readbackLead.accountKey, provider: readbackLead.provider, model: readbackLead.model }, workerDefaults: readbackLead.workerDefaults, request: postedBody }, null, 2)}\n`,
    );
  } catch (error) {
    journal.failure = { message: error.message, at: new Date().toISOString() };
    saveJournal(journalPath, journal);
    throw error;
  } finally {
    await context.close();
  }
}

main().catch((error) => {
  process.stderr.write(`${error.stack || error}\n`);
  process.exitCode = 1;
});
