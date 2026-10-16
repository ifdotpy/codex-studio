#!/usr/bin/env node
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const LEAD_PROVIDER = "claude";
const LEAD_MODEL = "default";
const DEFAULT_WORKER_ACCOUNT_KEY = "default";
const WORKER_MODEL = "gpt-6-luna";
const REVIEW_MODEL = "gpt-6.1-sol";
const EFFORT = "high";
const ACTION_TIMEOUT_MS = 15_000;
const NAVIGATION_TIMEOUT_MS = 45_000;
const READBACK_TIMEOUT_MS = 30_000;

function argsFrom(argv) {
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
    "  node scripts/verify-rust-worker-defaults-create.mjs --url HTTPS_URL --project PROJECT_PATH --lead-account ACCOUNT_KEY [--worker-account ACCOUNT_KEY] --evidence-dir DIR --chromium PATH",
    "  node scripts/verify-rust-worker-defaults-create.mjs --url HTTPS_URL --project PROJECT_PATH --lead-account ACCOUNT_KEY [--worker-account ACCOUNT_KEY] --evidence-dir DIR --resume --chromium PATH",
    "",
    "Creates one empty chat through the UI, verifies its defaults, then deletes only that journal-owned chat through the UI.",
    "Resume is readback-only and never repeats create, settings, prompt, or delete writes.",
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

function uuid(value) {
  return (
    typeof value === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(
      value,
    )
  );
}

async function readServerAgent(page, id, expectedOrigin, expectedServerId) {
  const result = await page.evaluate(
    async ({ agentId, expectedOrigin, expectedServerId, timeoutMs }) => {
      if (location.origin !== expectedOrigin)
        throw new Error("The page origin changed before sync readback.");
      const identityResponse = await fetch(
        new URL("/api/multi-server", expectedOrigin),
        {
          credentials: "same-origin",
          signal: AbortSignal.timeout(timeoutMs),
        },
      );
      let identityBody = null;
      try {
        identityBody = await identityResponse.json();
      } catch {}
      if (
        identityResponse.status !== 200 ||
        identityBody?.identity?.serverId !== expectedServerId
      )
        throw new Error("Server identity changed before sync readback.");
      const url = new URL("/api/sync/pull", expectedOrigin);
      url.searchParams.set("after", "0");
      url.searchParams.set("fresh", "1");
      url.searchParams.set("reset", "1");
      url.searchParams.set("priorityId", agentId);
      const response = await fetch(url, {
        credentials: "same-origin",
        signal: AbortSignal.timeout(timeoutMs),
      });
      let body = null;
      try {
        body = await response.json();
      } catch {}
      return {
        origin: url.origin,
        serverId: identityBody.identity.serverId,
        status: response.status,
        documents: body?.documents || [],
      };
    },
    {
      agentId: id,
      expectedOrigin,
      expectedServerId,
      timeoutMs: READBACK_TIMEOUT_MS,
    },
  );
  assert.equal(
    result.origin,
    expectedOrigin,
    "Authoritative sync read used another server origin.",
  );
  assert.equal(
    result.serverId,
    expectedServerId,
    "Authoritative sync read used another server identity.",
  );
  assert.equal(
    result.status,
    200,
    `Authoritative sync read returned HTTP ${result.status}.`,
  );
  const document = result.documents.find((item) => {
    if (item.id === id || item.id?.endsWith(`:${id}`)) return true;
    if (typeof item.payload !== "string") return false;
    try {
      const entity = JSON.parse(item.payload);
      return entity?.collection === "agent" && entity.id === id;
    } catch {
      return false;
    }
  });
  if (!document) return null;
  if (document._deleted) return { deleted: true, seq: document.seq };
  assert.equal(
    typeof document.payload,
    "string",
    "Sync document has no payload.",
  );
  const entity = JSON.parse(document.payload);
  return { deleted: false, seq: document.seq, agent: entity.value };
}

async function readLocalServerIdentity(page, expectedOrigin) {
  const result = await page.evaluate(
    async ({ origin, timeoutMs }) => {
      if (location.origin !== origin)
        throw new Error("The page origin changed before server preflight.");
      const response = await fetch(new URL("/api/multi-server", origin), {
        credentials: "same-origin",
        signal: AbortSignal.timeout(timeoutMs),
      });
      let body = null;
      try {
        body = await response.json();
      } catch {}
      return {
        origin: location.origin,
        status: response.status,
        identity: body?.identity,
      };
    },
    { origin: expectedOrigin, timeoutMs: READBACK_TIMEOUT_MS },
  );
  assert.equal(
    result.origin,
    expectedOrigin,
    "Server identity read used another origin.",
  );
  assert.equal(result.status, 200, "Could not read the local server identity.");
  assert.ok(
    typeof result.identity?.serverId === "string" && result.identity.serverId,
    "Local server identity has no server ID.",
  );
  assert.ok(
    typeof result.identity?.label === "string" && result.identity.label,
    "Local server identity has no label.",
  );
  return {
    origin: result.origin,
    serverId: result.identity.serverId,
    label: result.identity.label,
  };
}

function recordBlockedAttempt(journalPath, journal, operation, url, reason) {
  const target = new URL(url);
  journal.blockedAttempts ||= [];
  journal.blockedAttempts.push({
    operation,
    origin: target.origin,
    path: target.pathname,
    reason,
    at: new Date().toISOString(),
  });
  saveJournal(journalPath, journal);
}

async function pinLocalServer(page, origin, expectedServerId) {
  const identity = await readLocalServerIdentity(page, origin);
  assert.equal(
    identity.serverId,
    expectedServerId,
    "The preflight local server identity changed before the write.",
  );
}

function matchesMutationResponse(response, origin, path, id) {
  try {
    const request = response.request();
    const url = new URL(response.url());
    return (
      request.method() === "POST" &&
      url.origin === origin &&
      url.pathname === path &&
      request.postDataJSON()?.id === id
    );
  } catch {
    return false;
  }
}

function assertDesiredAgent(agent, id, leadAccountKey, workerAccountKey) {
  assert.equal(agent?.id, id, "Server returned another chat identity.");
  assert.equal(agent?.isLead, true, "Created chat is not a lead.");
  assert.equal(
    agent?.accountKey,
    leadAccountKey,
    "Lead account does not match the requested Claude account.",
  );
  assert.equal(agent?.provider, LEAD_PROVIDER, "Lead provider is not Claude.");
  assert.equal(
    agent?.model,
    LEAD_MODEL,
    "Lead model is not the default Claude model.",
  );
  assert.equal(
    agent?.workerDefaults?.accountKey,
    workerAccountKey,
    "Worker account does not match the requested account.",
  );
  assert.equal(
    agent?.workerDefaults?.model,
    WORKER_MODEL,
    "Worker model is not Luna.",
  );
  assert.equal(
    agent?.workerDefaults?.effort,
    EFFORT,
    "Worker reasoning is not High.",
  );
  assert.equal(
    agent?.workerDefaults?.fastMode,
    false,
    "Worker Fast mode changed.",
  );
  assert.equal(
    agent?.workerDefaults?.daybreakEnabled,
    false,
    "Worker Daybreak changed.",
  );
  assert.equal(
    agent?.workerDefaults?.delegationDepth,
    1,
    "Worker delegation depth changed.",
  );
  assert.equal(
    agent?.reviewDefaults?.model,
    REVIEW_MODEL,
    "Review model is not Sol.",
  );
  assert.equal(
    agent?.reviewDefaults?.effort,
    EFFORT,
    "Review reasoning is not High.",
  );
  return {
    id: agent.id,
    accountKey: agent.accountKey,
    provider: agent.provider,
    model: agent.model,
    workerDefaults: agent.workerDefaults,
    reviewDefaults: agent.reviewDefaults,
  };
}

function assertCreateRequest(body, journal) {
  assert.ok(uuid(body?.id), "New chat request has no UUID identity.");
  assert.ok(
    !journal.baseline.chatIds.includes(body.id),
    "Refusing to reuse an existing chat ID.",
  );
  assert.ok(
    !journal.baseline.pendingCreationIds.includes(body.id),
    "Refusing a pending user chat ID.",
  );
  assert.equal(
    body.cwd,
    journal.projectPath,
    "New chat request targets another project.",
  );
  assert.equal(
    body.reuse_empty,
    false,
    "Refusing to reuse an existing empty chat.",
  );
  assert.equal(
    body.account_key,
    journal.leadAccountKey,
    "New chat lead account differs from the requested account.",
  );
  assert.equal(body.model, LEAD_MODEL, "New chat lead model is not default.");
  assert.deepEqual(
    body.worker_defaults,
    {
      account_key: journal.workerAccountKey,
      model: WORKER_MODEL,
      effort: EFFORT,
      fast_mode: false,
      daybreak_enabled: false,
      delegation_depth: 1,
    },
    "New chat worker defaults differ from the requested payload.",
  );
  assert.deepEqual(
    body.review_defaults,
    { model: REVIEW_MODEL, effort: EFFORT },
    "New chat review defaults differ from the requested payload.",
  );
  assert.equal(
    "prompt" in body,
    false,
    "Refusing to create a chat with a prompt.",
  );
  assert.equal(
    "message" in body,
    false,
    "Refusing to create a chat with a message.",
  );
  return body;
}

async function baseline(page) {
  const chatIds = await page
    .locator("[data-chat]")
    .evaluateAll((rows) =>
      rows.map((row) => row.getAttribute("data-chat")).filter(Boolean),
    );
  const pendingCreationIds = await page.evaluate(() => {
    const ids = [];
    for (let index = 0; index < localStorage.length; index += 1) {
      const key = localStorage.key(index);
      if (!key?.startsWith("codex-pending-creation:")) continue;
      try {
        const request = JSON.parse(localStorage.getItem(key) || "null");
        if (typeof request?.id === "string") ids.push(request.id);
      } catch {}
    }
    return ids;
  });
  return {
    capturedAt: new Date().toISOString(),
    chatIds: [...new Set(chatIds)],
    pendingCreationIds: [...new Set(pendingCreationIds)],
  };
}

async function main() {
  const options = argsFrom(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(`${usage()}\n`);
    return;
  }
  assert.ok(options.url, "--url is required.");
  assert.ok(options.project, "--project is required.");
  assert.ok(options["lead-account"], "--lead-account is required.");
  assert.ok(options["evidence-dir"], "--evidence-dir is required.");
  const leadAccountKey = options["lead-account"];
  const workerAccountKey =
    options["worker-account"] || DEFAULT_WORKER_ACCOUNT_KEY;
  assert.match(
    leadAccountKey,
    /^[A-Za-z0-9._-]+$/,
    "--lead-account contains unsupported characters.",
  );
  assert.match(
    workerAccountKey,
    /^[A-Za-z0-9._-]+$/,
    "--worker-account contains unsupported characters.",
  );
  const origin = ensureHttps(options.url);
  const projectPath = resolve(options.project);
  const evidenceDir = resolve(options["evidence-dir"]);
  const profileDir = resolve(
    options["profile-dir"] || join(evidenceDir, "profile"),
  );
  const journalPath = join(evidenceDir, "rust-worker-defaults-create.json");
  const existing = readJournal(journalPath);
  if (options.resume)
    assert.ok(existing, "Resume requires the saved ownership journal.");
  else
    assert.equal(
      existing,
      null,
      "Evidence already exists; use --resume, which never writes.",
    );
  const journal = existing || {
    version: 1,
    origin,
    projectPath,
    leadAccountKey,
    workerAccountKey,
    createdAt: new Date().toISOString(),
    create: { state: "not-sent" },
    deletion: { state: "not-sent" },
  };
  assert.equal(journal.version, 1, "Unsupported journal version.");
  assert.equal(journal.origin, origin, "Evidence belongs to another server.");
  assert.equal(
    journal.projectPath,
    projectPath,
    "Evidence belongs to another project.",
  );
  assert.equal(
    journal.leadAccountKey,
    leadAccountKey,
    "Evidence belongs to another lead account.",
  );
  assert.equal(
    journal.workerAccountKey,
    workerAccountKey,
    "Evidence belongs to another worker account.",
  );
  if (options.resume && journal.create?.state === "not-sent")
    throw new Error(
      "No create request was sent; start with a new evidence directory.",
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
  let blockedWrite = null;

  await page.route("**/api/leads", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    if (options.resume) {
      blockedWrite = "Resume mode blocks chat creation.";
      await route.abort();
      return;
    }
    try {
      assert.equal(
        new URL(route.request().url()).origin,
        origin,
        "Chat creation must target the preflight local server origin.",
      );
      await pinLocalServer(page, origin, journal.localServer.serverId);
      const body = assertCreateRequest(route.request().postDataJSON(), journal);
      if (journal.create.state !== "not-sent")
        throw new Error("Chat creation was already attempted.");
      journal.create = {
        state: "sent",
        id: body.id,
        request: body,
        sentAt: new Date().toISOString(),
      };
      saveJournal(journalPath, journal);
      await route.continue();
    } catch (error) {
      blockedWrite = error.message;
      recordBlockedAttempt(
        journalPath,
        journal,
        "create",
        route.request().url(),
        error.message,
      );
      await route.fulfill({
        status: 409,
        contentType: "application/json",
        body: JSON.stringify({
          error: "Verifier blocked an unexpected chat creation.",
        }),
      });
    }
  });
  await page.route("**/api/conversation/delete", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    if (options.resume) {
      blockedWrite = "Resume mode blocks chat deletion.";
      await route.abort();
      journal.blockedOperations ||= [];
      journal.blockedOperations.push({
        path: "/api/conversation/delete",
        at: new Date().toISOString(),
      });
      saveJournal(journalPath, journal);
      return;
    }
    let body;
    try {
      body = route.request().postDataJSON();
      assert.equal(
        new URL(route.request().url()).origin,
        origin,
        "Chat deletion must target the preflight local server origin.",
      );
      await pinLocalServer(page, origin, journal.localServer.serverId);
      assert.equal(options.resume, false, "Resume mode never sends deletes.");
      assert.equal(
        journal.ownedCreated,
        true,
        "Delete is limited to a journal-owned chat.",
      );
      assert.equal(
        journal.create.state,
        "confirmed",
        "Create response is not confirmed.",
      );
      assert.equal(
        journal.readback?.state,
        "confirmed",
        "Created chat readback is not confirmed.",
      );
      assert.equal(body?.id, journal.create.id, "Delete targets another chat.");
      assert.ok(
        !journal.baseline.chatIds.includes(body.id),
        "Refusing to delete a baseline chat.",
      );
      assert.ok(
        !journal.baseline.pendingCreationIds.includes(body.id),
        "Refusing to delete a pending user chat.",
      );
      if (journal.deletion.state !== "not-sent")
        throw new Error("Delete was already attempted.");
      journal.deletion = {
        state: "sent",
        request: body,
        sentAt: new Date().toISOString(),
      };
      saveJournal(journalPath, journal);
      await route.continue();
    } catch (error) {
      blockedWrite = error.message;
      recordBlockedAttempt(
        journalPath,
        journal,
        "delete",
        route.request().url(),
        error.message,
      );
      await route.fulfill({
        status: 409,
        contentType: "application/json",
        body: JSON.stringify({
          error: "Verifier blocked an unexpected delete.",
        }),
      });
    }
  });
  for (const path of ["/api/messages", "/api/conversation"]) {
    await page.route(`**${path}`, async (route) => {
      if (route.request().method() !== "POST") return route.continue();
      blockedWrite = `Prompt and settings writes are disabled at ${path}.`;
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
    const localServer = await readLocalServerIdentity(page, origin);
    if (options.resume) {
      assert.equal(
        journal.localServer?.origin,
        localServer.origin,
        "Evidence belongs to another local server origin.",
      );
      assert.equal(
        journal.localServer?.serverId,
        localServer.serverId,
        "Evidence belongs to another local server identity.",
      );
    } else {
      journal.localServer = localServer;
      saveJournal(journalPath, journal);
    }
    if (options.resume) {
      const id = journal.create.id;
      assert.ok(uuid(id), "Journal has no safe created chat identity.");
      const snapshot = await readServerAgent(
        page,
        id,
        origin,
        journal.localServer.serverId,
      );
      journal.resumeReadback = {
        observed: snapshot?.deleted
          ? "deleted"
          : snapshot
            ? "present"
            : "not in current sync projection",
        isDesired: snapshot?.agent
          ? (() => {
              try {
                assertDesiredAgent(
                  snapshot.agent,
                  id,
                  leadAccountKey,
                  workerAccountKey,
                );
                return true;
              } catch {
                return false;
              }
            })()
          : false,
        at: new Date().toISOString(),
        writeOutcome: "readback only; no create or delete was replayed",
      };
      saveJournal(journalPath, journal);
      process.stdout.write(
        `${JSON.stringify({ chatId: id, ...journal.resumeReadback }, null, 2)}\n`,
      );
      return;
    }

    const projectSections = page.locator("[data-project-path]");
    await projectSections.first().waitFor();
    const actualPath = await projectSections.evaluateAll(
      (sections, wanted) =>
        sections
          .find(
            (section) => section.getAttribute("data-project-path") === wanted,
          )
          ?.getAttribute("data-project-path") || null,
      projectPath,
    );
    assert.equal(
      actualPath,
      projectPath,
      "Requested project path is not present in the UI.",
    );
    const projectSection = page.locator(
      `[data-project-path=${JSON.stringify(projectPath)}]`,
    );
    assert.equal(
      await projectSection.count(),
      1,
      "Project path is not unique in the UI.",
    );
    const projectName = await projectSection
      .locator(".project-tree-toggle > span[title]")
      .getAttribute("title");
    assert.ok(projectName, "Project row has no name.");
    journal.projectName = projectName;
    journal.baseline = await baseline(page);
    saveJournal(journalPath, journal);
    await projectSection.locator(".project-tree-heading").hover();
    await projectSection
      .getByRole("button", {
        name: `New chat in ${projectName.trim()}`,
        exact: true,
      })
      .first()
      .click();
    const dialog = page.getByRole("dialog", { name: "New chat", exact: true });
    await dialog.waitFor();
    const serverCards = dialog
      .getByRole("group", { name: "Chat server", exact: true })
      .getByRole("button");
    const count = await serverCards.count();
    const matchingLocalCards = [];
    for (let index = 0; index < count; index += 1) {
      const card = serverCards.nth(index);
      const label = await card.locator("strong").innerText();
      if (label.trim() === localServer.label) matchingLocalCards.push(card);
    }
    assert.equal(
      matchingLocalCards.length,
      1,
      `Expected one local server card labeled ${localServer.label}.`,
    );
    const matchingCard = matchingLocalCards[0];
    const localCardLines = (await matchingCard.innerText())
      .split("\n")
      .map((line) => line.trim());
    assert.ok(
      localCardLines.includes(projectPath),
      `Project ${projectPath} is not available on the local server.`,
    );
    if ((await matchingCard.getAttribute("aria-pressed")) !== "true")
      await matchingCard.click();
    assert.equal(
      await matchingCard.getAttribute("aria-pressed"),
      "true",
      "The local server is not selected for chat creation.",
    );

    const mainSettings = dialog.getByRole("button", {
      name: "Main agent settings",
      exact: true,
    });
    await mainSettings.click();
    const mainSettingsDialog = page.getByRole("dialog", {
      name: "Main agent settings",
      exact: true,
    });
    const mainAccount = mainSettingsDialog.getByRole("group", {
      name: "Main agent account",
      exact: true,
    });
    await mainAccount
      .getByLabel("Main agent account provider", { exact: true })
      .locator("label")
      .filter({ hasText: "Claude" })
      .click();
    const claudeAccount = mainAccount.locator(
      `[data-account-key="${leadAccountKey}"]`,
    );
    assert.equal(
      await claudeAccount.count(),
      1,
      "Expected exactly one requested Claude account tile.",
    );
    await claudeAccount.click();
    const orchestrator = mainSettingsDialog
      .getByRole("group", { name: "Agent role", exact: true })
      .getByRole("button", { name: "Orchestrator", exact: true });
    await orchestrator.click();
    const mainModel = mainSettingsDialog.getByRole("listbox", {
      name: "Main agent model",
      exact: true,
    });
    const defaultLeadModel = mainModel.locator(
      '[role="option"][data-value="default"]',
    );
    await defaultLeadModel.waitFor();
    await defaultLeadModel.click();
    await mainSettingsDialog
      .locator(
        '[role="listbox"][aria-label="Main agent model"][data-value="default"]',
      )
      .waitFor();

    await dialog
      .getByRole("button", { name: "Subagent defaults", exact: true })
      .click();
    const subagentDialog = page.getByRole("dialog", {
      name: "Subagent defaults",
      exact: true,
    });
    const roles = subagentDialog.getByRole("group", {
      name: "Agent role",
      exact: true,
    });
    await roles.getByRole("button", { name: "Worker", exact: true }).click();
    const workerAccount = subagentDialog.getByRole("group", {
      name: "Subagent account",
      exact: true,
    });
    await workerAccount
      .getByLabel("Subagent account provider", { exact: true })
      .locator("label")
      .filter({ hasText: "Codex" })
      .click();
    const codexAccount = workerAccount.locator(
      `[data-account-key="${workerAccountKey}"]`,
    );
    assert.equal(
      await codexAccount.count(),
      1,
      "Default Codex account tile is missing or duplicated.",
    );
    await codexAccount.click();
    const selectedCodexAccount = workerAccount.locator(
      `[data-account-key="${workerAccountKey}"][aria-pressed="true"]`,
    );
    await selectedCodexAccount.waitFor();
    assert.equal(
      await selectedCodexAccount.getAttribute("aria-pressed"),
      "true",
      "Worker account is not pinned to Default.",
    );
    const workerModel = subagentDialog.getByRole("listbox", {
      name: "Default subagent model",
      exact: true,
    });
    await workerModel
      .locator(`[role="option"][data-value="${WORKER_MODEL}"]`)
      .click();
    await workerModel
      .locator(
        `[role="option"][data-value="${WORKER_MODEL}"][aria-selected="true"]`,
      )
      .waitFor();
    await subagentDialog
      .locator(
        `[role="radiogroup"][aria-label="Default subagent reasoning"][data-value="${EFFORT}"]`,
      )
      .waitFor();
    assert.equal(
      await subagentDialog
        .getByLabel("Default subagent reasoning", { exact: true })
        .getAttribute("data-value"),
      EFFORT,
      "Worker effort is not High.",
    );

    await roles.getByRole("button", { name: "Review", exact: true }).click();
    const reviewDialog = page.getByRole("dialog", {
      name: "Review settings",
      exact: true,
    });
    const reviewModelList = reviewDialog.getByRole("listbox", {
      name: "Default review model",
      exact: true,
    });
    const reviewModel = reviewModelList.locator(
      `[role="option"][data-value="${REVIEW_MODEL}"]`,
    );
    await reviewModel.waitFor();
    await reviewModel.click();
    await reviewModelList
      .locator(
        `[role="option"][data-value="${REVIEW_MODEL}"][aria-selected="true"]`,
      )
      .waitFor();
    const reviewEffort = reviewDialog.getByLabel("Default review reasoning", {
      exact: true,
    });
    await reviewEffort.locator(`[data-value="${EFFORT}"]`).click();
    await reviewDialog
      .locator(
        `[role="radiogroup"][aria-label="Default review reasoning"][data-value="${EFFORT}"]`,
      )
      .waitFor();

    assert.ok(!blockedWrite, blockedWrite || undefined);
    journal.preflight =
      "Visible settings target the local server and show Claude default lead, pinned Codex Luna High workers, and Sol High review before one create POST.";
    saveJournal(journalPath, journal);
    const creationResponse = page.waitForResponse(
      (response) =>
        matchesMutationResponse(
          response,
          origin,
          "/api/leads",
          journal.create.id,
        ),
      { timeout: READBACK_TIMEOUT_MS },
    );
    await dialog
      .getByRole("button", { name: "Start chat", exact: true })
      .click();
    const response = await creationResponse;
    assert.equal(blockedWrite, null, blockedWrite || undefined);
    const canonical = await response.json().catch(() => null);
    journal.create = {
      ...journal.create,
      state: response.status() === 200 ? "response-received" : "response-error",
      status: response.status(),
      responseAt: new Date().toISOString(),
      canonical: canonical
        ? {
            id: canonical.id,
            accountKey: canonical.accountKey,
            provider: canonical.provider,
            model: canonical.model,
            workerDefaults: canonical.workerDefaults,
            reviewDefaults: canonical.reviewDefaults,
          }
        : null,
    };
    saveJournal(journalPath, journal);
    assert.equal(
      response.status(),
      200,
      `Chat creation returned HTTP ${response.status()}; do not retry.`,
    );
    assert.equal(
      canonical?.id,
      journal.create.id,
      "Canonical response identity differs from journaled request.",
    );
    assertDesiredAgent(
      canonical,
      journal.create.id,
      leadAccountKey,
      workerAccountKey,
    );
    journal.create.state = "confirmed";
    saveJournal(journalPath, journal);

    await page
      .locator(`[data-chat=${JSON.stringify(journal.create.id)}]`)
      .waitFor();
    const synced = await readServerAgent(
      page,
      journal.create.id,
      origin,
      journal.localServer.serverId,
    );
    assert.ok(
      synced && !synced.deleted,
      "Authoritative sync does not contain the newly created chat.",
    );
    const readback = assertDesiredAgent(
      synced.agent,
      journal.create.id,
      leadAccountKey,
      workerAccountKey,
    );
    assert.ok(
      !journal.baseline.chatIds.includes(journal.create.id),
      "Created ID existed in the pre-write chat list.",
    );
    assert.ok(
      !journal.baseline.pendingCreationIds.includes(journal.create.id),
      "Created ID existed in the pre-write pending list.",
    );
    journal.ownedCreated = true;
    journal.readback = {
      state: "confirmed",
      seq: synced.seq,
      agent: readback,
      at: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);

    const row = page.locator(
      `[data-sidebar-item=${JSON.stringify(journal.create.id)}]`,
    );
    await row.locator(".row-actions").click();
    await page.getByRole("menuitem", { name: "Delete", exact: true }).click();
    const confirm = page.getByRole("dialog", {
      name: "Delete chat?",
      exact: true,
    });
    const deleteResponsePromise = page.waitForResponse(
      (candidate) =>
        matchesMutationResponse(
          candidate,
          origin,
          "/api/conversation/delete",
          journal.create.id,
        ),
      { timeout: READBACK_TIMEOUT_MS },
    );
    await confirm
      .getByRole("button", { name: "Delete chat", exact: true })
      .click();
    const deleteResponse = await deleteResponsePromise;
    const deleteBody = await deleteResponse.json().catch(() => null);
    assert.equal(
      deleteResponse.status(),
      200,
      `Owned chat cleanup returned HTTP ${deleteResponse.status()}; do not retry.`,
    );
    assert.deepEqual(
      deleteBody?.deleted,
      [journal.create.id],
      "Server did not confirm deletion of exactly the owned chat.",
    );
    journal.deletion = {
      ...journal.deletion,
      state: "response-received",
      status: deleteResponse.status(),
      deleted: deleteBody.deleted,
      at: new Date().toISOString(),
    };
    saveJournal(journalPath, journal);
    await row.waitFor({ state: "detached", timeout: READBACK_TIMEOUT_MS });
    const deletionReadback = await readServerAgent(
      page,
      journal.create.id,
      origin,
      journal.localServer.serverId,
    );
    assert.ok(
      !deletionReadback || deletionReadback.deleted,
      "Authoritative sync still contains the deleted chat.",
    );
    journal.deletion.state = "confirmed";
    journal.deletion.readbackAt = new Date().toISOString();
    saveJournal(journalPath, journal);
    process.stdout.write(
      `${JSON.stringify({ state: "passed", projectPath, chatId: journal.create.id, lead: { accountKey: readback.accountKey, provider: readback.provider, model: readback.model }, workerDefaults: readback.workerDefaults, reviewDefaults: readback.reviewDefaults, cleanup: journal.deletion.state }, null, 2)}\n`,
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
