/** Read-only native usage evidence and OLD/NEW presentation for installed smoke. */
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { isDeepStrictEqual } from "node:util";
import { execFileSync } from "node:child_process";

// Owner: runtime-services/src/pricing.rs; parity checked by installed-smoke tests.
// Ratio of one-hour cache-write USD/million to the selected input USD/million.
export const CLAUDE_CACHE_WRITE_1H_INPUT_MULTIPLIER = 2.0;

export const NOTES_FILE = "notes.txt";
export const NOTES_MARKER = "STUDIO_USAGE_READ_ONLY_NOTE";

export const TOKEN_CATEGORIES = [
  "uncachedInput",
  "cacheReadInput",
  "cacheWriteInput",
  "output",
  "reasoningOutput",
];

/** Codex includes cache reads in input; Claude reports uncached input separately. */
export function nativeTokens(usage, provider) {
  const input = usage.input_tokens ?? usage.inputTokens ?? 0;
  const cacheReadInput =
    usage.cache_read_input_tokens ??
    usage.cached_input_tokens ??
    usage.cachedInputTokens ??
    0;
  return {
    uncachedInput:
      provider === "claude"
        ? input
        : input -
          cacheReadInput -
          (usage.cache_write_input_tokens ?? usage.cacheWriteInputTokens ?? 0),
    cacheReadInput,
    cacheWriteInput:
      usage.cache_creation_input_tokens ??
      usage.cache_write_input_tokens ??
      usage.cacheWriteInputTokens ??
      0,
    ...(provider === "claude"
      ? {
          cacheWriteInput1h: Math.min(
            usage.cache_creation?.ephemeral_1h_input_tokens ?? 0,
            usage.cache_creation_input_tokens ?? 0,
          ),
        }
      : {}),
    output: usage.output_tokens ?? usage.outputTokens ?? 0,
    reasoningOutput:
      usage.reasoning_output_tokens ?? usage.reasoningOutputTokens ?? 0,
  };
}

export function sumTokens(rows) {
  return {
    ...Object.fromEntries(
      TOKEN_CATEGORIES.map((key) => [
        key,
        rows.some((row) => row.tokens[key] == null)
          ? null
          : rows.reduce((sum, row) => sum + row.tokens[key], 0),
      ]),
    ),
    // Contract v4: an absent lifetime split contributes zero one-hour writes.
    cacheWriteInput1h: rows.reduce(
      (sum, row) => sum + (row.tokens.cacheWriteInput1h ?? 0),
      0,
    ),
  };
}

export function totalTokens(tokens) {
  if (TOKEN_CATEGORIES.slice(0, 4).some((key) => tokens[key] == null))
    return null;
  return (
    tokens.uncachedInput +
    tokens.cacheReadInput +
    tokens.cacheWriteInput +
    tokens.output
  );
}

/** Final journal entry wins per Claude message id; partial content blocks repeat that id. */
export function nativeResponses(records, provider) {
  const responses = new Map();
  let model;
  for (const row of records) {
    if (row.type === "turn_context") model = row.payload?.model;
    if (
      provider === "claude" &&
      row.type === "assistant" &&
      row.message?.id &&
      row.message?.usage &&
      row.message.model !== "<synthetic>"
    ) {
      responses.set(row.message.id, {
        responseId: row.message.id,
        model: row.message.model,
        tokens: nativeTokens(row.message.usage, provider),
        rawUsage: row.message.usage,
      });
    }
    if (
      provider === "codex" &&
      row.type === "token_usage_record" &&
      row.payload?.response_id
    ) {
      const p = row.payload;
      responses.set(p.response_id, {
        responseId: p.response_id,
        turnId: p.turn_id,
        model: p.model ?? model,
        tokens: nativeTokens(p.usage, provider),
        rawUsage: p.usage,
      });
    }
  }
  return [...responses.values()];
}

function jsonLines(file) {
  return fs
    .readFileSync(file, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

/** Discover only this thread's files. Never scan or copy other chats' contents. */
export function nativeEvidence(state, agent, account, history) {
  const files = new Set();
  if (agent.provider === "claude") {
    const serverRoot =
      account.id === "default"
        ? state
        : path.join(state, "account-servers", account.id);
    const snapshot = path.join(
      serverRoot,
      "sessions",
      `${agent.threadId}.json`,
    );
    const saved = JSON.parse(fs.readFileSync(snapshot, "utf8"));
    const session = saved.session ?? saved;
    const ids = new Set([agent.threadId, session.nativeId]);
    for (const branch of session.historyBranches ?? [])
      ids.add(branch.nativeId);
    const config =
      account.claudeOptions?.configDir ??
      account.home ??
      path.join(os.homedir(), ".claude");
    const projects = path.join(
      config.replace(/^~(?=\/|$)/, os.homedir()),
      "projects",
    );
    for (const project of fs.readdirSync(projects, { withFileTypes: true })) {
      if (!project.isDirectory()) continue;
      for (const id of ids) {
        if (!id || path.basename(id) !== id) continue;
        const file = path.join(projects, project.name, `${id}.jsonl`);
        if (fs.existsSync(file)) files.add(file);
      }
    }
  } else {
    for (const entry of history)
      if (entry.threadId === agent.threadId && entry.path)
        files.add(entry.path);
    if (!files.size) {
      const sessions = path.join(account.home, "sessions");
      const walk = (folder, depth) => {
        if (depth < 0 || !fs.existsSync(folder)) return;
        for (const entry of fs.readdirSync(folder, { withFileTypes: true })) {
          const file = path.join(folder, entry.name);
          if (entry.isDirectory()) walk(file, depth - 1);
          else if (entry.name.endsWith(`-${agent.threadId}.jsonl`))
            files.add(file);
        }
      };
      walk(sessions, 3);
    }
  }
  if (!files.size)
    throw new Error(
      `No native usage evidence found for ${agent.id} thread ${agent.threadId}`,
    );
  const responses = nativeResponses(
    [...files].flatMap(jsonLines),
    agent.provider,
  );
  if (!responses.length)
    throw new Error(
      `No final native response usage found in ${[...files].join(", ")}`,
    );
  return { files: [...files], responses, tokens: sumTokens(responses) };
}

/** Canonical observations take precedence, including identity and non-response kinds. */
export function recordedUsage(analytics) {
  const responses = [];
  const gaps = [];
  for (const row of analytics.timeline ?? []) {
    if (Object.hasOwn(row, "studioUsage")) {
      const observation = row.studioUsage;
      if (observation?.kind === "response") {
        responses.push({ ...observation, record: row });
      } else if (observation?.kind === "gap") {
        gaps.push({ ...observation, tokens: null, costUsd: null, record: row });
      }
      continue;
    }
    // Pre-contract persisted rows only: mirror runtime-schema/src/usage/persisted.rs.
    // Retained for OLD evidence and bridges that started before typed usage delivery.
    if (
      !row.responseId ||
      ["nativeNotice", "codexThread", "claudeQuery"].includes(row.counterDomain)
    )
      continue;
    const known = (key) =>
      Number.isSafeInteger(selected?.[key]) && selected[key] >= 0;
    const selected =
      row.delta?.inputTokens != null && row.delta?.outputTokens != null
        ? row.delta
        : row.requestUsage && typeof row.requestUsage === "object"
          ? row.requestUsage
          : row.last;
    const input = known("inputTokens") ? selected.inputTokens : null;
    const read = known("cachedInputTokens") ? selected.cachedInputTokens : null;
    const write = known("cacheWriteInputTokens")
      ? selected.cacheWriteInputTokens
      : null;
    const uncached =
      row.inputTokensAreUncached === true ||
      row.usageSource === "claudeResponse";
    const cacheReadInput =
      uncached || input == null || read == null ? read : Math.min(input, read);
    const cacheWriteInput =
      uncached || input == null || cacheReadInput == null || write == null
        ? write
        : Math.min(input - cacheReadInput, write);
    responses.push({
      responseId: row.responseId,
      turnId: row.turnId,
      model: row.model,
      tokens: {
        uncachedInput: uncached
          ? input
          : [input, cacheReadInput, cacheWriteInput].includes(null)
            ? null
            : input - cacheReadInput - cacheWriteInput,
        cacheReadInput,
        cacheWriteInput,
        output: known("outputTokens") ? selected.outputTokens : null,
        reasoningOutput: known("reasoningOutputTokens")
          ? selected.reasoningOutputTokens
          : null,
      },
      evidence: "legacy",
      record: row,
    });
  }
  return { responses, gaps };
}

export function recordedResponses(analytics) {
  return recordedUsage(analytics).responses;
}

export function usageTable(evidence, label) {
  return evidence.providers.map((row) => ({
    run: label,
    provider: row.provider,
    agent: row.agentId,
    "truth input/read/write/output/reasoning": TOKEN_CATEGORIES.map(
      (key) => row.truth.tokens[key],
    ).join("/"),
    "recorded input/read/write/output/reasoning": row.recorded.gaps?.length
      ? null
      : TOKEN_CATEGORIES.map(
          (key) => row.recorded.tokens[key] ?? "unknown",
        ).join("/"),
    "recorded total tokens": row.recorded.gaps?.length
      ? null
      : totalTokens(row.recorded.tokens),
    "partial recorded input/read/write/output/reasoning": row.recorded.gaps
      ?.length
      ? TOKEN_CATEGORIES.map(
          (key) => row.recorded.tokens[key] ?? "unknown",
        ).join("/")
      : null,
    "recorded/truth":
      row.recorded.gaps?.length ||
      totalTokens(row.recorded.tokens) == null ||
      !totalTokens(row.truth.tokens)
        ? null
        : totalTokens(row.recorded.tokens) / totalTokens(row.truth.tokens),
    "unpriced gaps": row.recorded.gaps?.length ?? null,
    "recorded projection": row.recorded.projection ?? "persisted-usage-v3",
    "budget tokens": row.budget.tokensUsed,
    accounting: row.budget.tokenUsageAccounting,
    "ratio basis": "billed tokens; reasoning already included in output",
    "pricing basis": evidence.pricingBasis ?? "not recomputed",
    "recorded USD": row.recorded.recomputedCostUsd ?? null,
    "truth USD": row.truth.recomputedCostUsd ?? null,
    "saved truth USD": row.truth.costUsd ?? null,
    "CLI provider USD":
      row.provider === "claude"
        ? (row.reconciliation?.providerCosts?.reduce(
            (sum, model) => sum + model.providerCostUsd,
            0,
          ) ?? null)
        : null,
    "saved displayed USD": row.displayedCost?.totalUSD ?? null,
    "analytics latency":
      row.analyticsLatency
        ?.map(
          (timing) =>
            `${timing.outcome}:${timing.elapsedMs ?? timing.timeoutMs}ms`,
        )
        .join(", ") ?? null,
    "saved reconciled recorded USD":
      row.reconciliation?.totals?.recordedCostUsd ?? null,
    "saved reconciled evidence USD":
      row.reconciliation?.totals?.evidenceCostUsd ?? null,
  }));
}

/** Re-project both saved runs without mutating snapshots or trusting old derived totals. */
export function compareEvidence(oldEvidence, newEvidence) {
  // One common basis for both runs: OLD's saved per-response rates, joined to
  // their native models. Missing or inconsistent rates cannot produce a price.
  const modelsByProvider = new Map();
  for (const row of oldEvidence.providers) {
    const models = modelsByProvider.get(row.provider) ?? new Map();
    modelsByProvider.set(row.provider, models);
    for (const price of row.truth?.prices ?? []) {
      const model = row.truth.responses?.find(
        (response) => response.responseId === price.responseId,
      )?.model;
      if (!model || !price.rates) continue;
      if (
        models.has(model) &&
        !isDeepStrictEqual(models.get(model), price.rates)
      )
        models.set(model, null);
      else if (!models.has(model)) models.set(model, price.rates);
    }
  }
  const reprice = (responses, provider) => {
    if (
      !Array.isArray(responses) ||
      responses.some((row) =>
        TOKEN_CATEGORIES.slice(0, 4).some(
          (key) =>
            !Number.isSafeInteger(row.tokens?.[key]) || row.tokens[key] < 0,
        ),
      )
    )
      return null;
    const models = Object.fromEntries(
      [...(modelsByProvider.get(provider) ?? [])].map(([model, cost]) => [
        model,
        { cost },
      ]),
    );
    try {
      return priceResponses(
        responses,
        {
          catalog: {
            providers: {
              [provider === "claude" ? "anthropic" : "openai"]: { models },
            },
          },
        },
        provider,
      ).costUsd;
    } catch {
      // Missing model/rate or out-of-base-tier usage is explicitly unpriced.
      return null;
    }
  };
  const project = (evidence) => ({
    ...evidence,
    pricingBasis:
      "OLD truth.prices rates by native model; USD per million tokens; base tiers only; missing/conflicting rates or incomplete usage = null",
    providers: evidence.providers.map((row) => {
      const available = Array.isArray(row.analytics?.timeline);
      const recorded = available ? recordedUsage(row.analytics) : null;
      return {
        ...row,
        truth: {
          ...row.truth,
          recomputedCostUsd: reprice(
            row.truth?.responses?.map((response) => ({
              ...response,
              tokens: response.rawUsage
                ? nativeTokens(response.rawUsage, row.provider)
                : response.tokens,
            })),
            row.provider,
          ),
        },
        recorded: recorded
          ? {
              ...recorded,
              tokens: sumTokens(recorded.responses),
              recomputedCostUsd: recorded.gaps.length
                ? null
                : reprice(recorded.responses, row.provider),
            }
          : {
              tokens: Object.fromEntries(
                TOKEN_CATEGORIES.map((key) => [key, null]),
              ),
              projection: `unknown: raw analytics unavailable (saved format ${evidence.format ?? "unversioned"})`,
            },
      };
    }),
  });
  return [
    ...usageTable(project(oldEvidence), "OLD"),
    ...usageTable(project(newEvidence), "NEW"),
  ];
}

/** Independent base-rate arithmetic; refuse tiered requests rather than guess. */
export function priceResponses(responses, catalog, provider) {
  const models =
    catalog.catalog.providers[provider === "claude" ? "anthropic" : "openai"]
      .models;
  const rows = responses.map((response) => {
    const cost = models[response.model]?.cost;
    if (!cost) throw new Error(`No price for native model ${response.model}`);
    const tokens = response.tokens;
    const context =
      tokens.uncachedInput + tokens.cacheReadInput + tokens.cacheWriteInput;
    const thresholds = (cost.tiers ?? [])
      .filter((tier) => tier.tier?.type === "context")
      .map((tier) => tier.tier.size)
      .filter(Number.isFinite);
    if (
      (cost.context_over_200k && !thresholds.length) ||
      thresholds.some((threshold) => context >= threshold)
    )
      throw new Error("Usage smoke exceeds base-rate pricing scope");
    const hour = tokens.cacheWriteInput1h ?? 0;
    const parts = [
      [tokens.uncachedInput, cost.input],
      [tokens.cacheReadInput, cost.cache_read],
      [tokens.cacheWriteInput - hour, cost.cache_write],
      [hour, cost.input * CLAUDE_CACHE_WRITE_1H_INPUT_MULTIPLIER],
      [tokens.output, cost.output],
    ];
    if (parts.some(([tokens, rate]) => tokens > 0 && !Number.isFinite(rate)))
      throw new Error(`Missing price category for ${response.model}`);
    return {
      responseId: response.responseId,
      rates: cost,
      costUsd:
        parts.reduce(
          (total, [tokens, rate]) => total + tokens * (rate ?? 0),
          0,
        ) / 1000000,
    };
  });
  return {
    costUsd: rows.reduce((total, row) => total + row.costUsd, 0),
    prices: rows,
  };
}

/** This budget label is absent from the installed API DTO; read only that field. */
export function accountingEvidence(state, agent) {
  const code = `import sqlite3,json,sys,pathlib
c=sqlite3.connect(pathlib.Path(sys.argv[1],'canvas.sqlite3').as_uri()+'?mode=ro',uri=True)
c.execute('PRAGMA query_only=ON')
r=c.execute('SELECT tokenUsageAccounting FROM runtime_agent_volatile WHERE id=?',(sys.argv[2],)).fetchone()
if r and r[0] is not None: print(r[0])
else:
 r=c.execute("SELECT json_extract(record,'$.tokenUsageAccounting') FROM runtime_agents WHERE id=?",(sys.argv[2],)).fetchone()
 print(json.dumps(r[0] if r else None))`;
  return JSON.parse(
    execFileSync("python3", ["-c", code, state, agent], {
      encoding: "utf8",
      timeout: 10000,
    }),
  );
}

/** Transcript proof of three turns, two long-turn responses, pwd and Claude steer. */
export function scenarioEvidence(items, provider, root) {
  const second = items.findIndex(
    (item) =>
      item.role === "user" &&
      item.text?.startsWith("This is turn 2 of a usage check."),
  );
  const third = items.findIndex(
    (item) =>
      item.role === "user" &&
      item.text?.startsWith("Turn 3: reply with exactly"),
  );
  if (second < 0 || third <= second)
    throw new Error("Usage scenario is missing turn 2 or turn 3");
  const long = items.slice(second + 1, third);
  const answers = long.filter(
    (item) => item.role === "assistant" && item.text?.trim(),
  );
  const tools = long
    .filter((item) => item.role === "output")
    .map((item) => ({
      id: item.id,
      turnId: item.turnId,
      toolStatus: item.toolStatus,
      ...JSON.parse(item.text),
    }));
  if (answers.length < 2)
    throw new Error("Long turn has fewer than two assistant responses");
  const readOnlyNote = items[second].text.includes(NOTES_FILE);
  const tool = tools[0];
  const file = path.join(root, NOTES_FILE);
  const bareCat = /^[A-Za-z0-9_./-]+$/.test(file) ? `cat ${file}` : null;
  const plainCat =
    (bareCat && tool?.command === bareCat) ||
    tool?.command === `cat '${file.replaceAll("'", "'\\''")}'` ||
    (!/[\\$`"]/.test(file) && tool?.command === `cat "${file}"`);
  const shellWrapper = /^\/bin\/(?:ba|z)?sh -(?:lc|c) (.+)$/.exec(
    tool?.command ?? "",
  );
  // Codex quotes the wrapped script as one argument: -lc 'cat /path'.
  const wrapped =
    /^'([^']*)'$/.exec(shellWrapper?.[1] ?? "")?.[1] ?? shellWrapper?.[1];
  const codexCat =
    provider === "codex" &&
    tool?.type === "commandExecution" &&
    tool.cwd === root &&
    (plainCat || (bareCat && wrapped === bareCat)) &&
    tool.exitCode === 0 &&
    tool.aggregatedOutput?.trim() === NOTES_MARKER;
  const claudeRead =
    provider === "claude" &&
    tool?.tool === "Read" &&
    tool.arguments?.file_path === file &&
    Object.keys(tool.arguments).every((key) =>
      ["file_path", "offset", "limit"].includes(key),
    ) &&
    JSON.stringify(tool.result ?? tool.output ?? "").includes(NOTES_MARKER);
  const validTool =
    tools.length === 1 &&
    (readOnlyNote
      ? tool.toolStatus === "completed" && (codexCat || claudeRead)
      : tool.exitCode === 0 &&
        /\bpwd$/.test(tool.command ?? "") &&
        tool.cwd === root &&
        tool.aggregatedOutput?.includes(root));
  if (!validTool)
    throw new Error(
      `Long turn did not complete exactly one ${readOnlyNote ? "note read" : "pwd call"} in the run folder`,
    );
  const steer = long.find(
    (item) => item.role === "user" && item.text?.includes("usage-check steer"),
  );
  if (provider === "claude" && !steer)
    throw new Error("Claude steer is absent from the transcript");
  return {
    longTurnId: tools[0].turnId,
    assistantResponses: answers.length,
    words: answers
      .map((item) => item.text)
      .join(" ")
      .trim()
      .split(/\s+/).length,
    tool: tools[0],
    steer: steer ? { id: steer.id, at: steer.at } : null,
  };
}

/** A displayed snapshot must match the fresh server computation covering final-turn usage. */
export function publicationReady(displayed, current, analytics, finalTurnId) {
  const ready = (cost) =>
    cost?.pricingState === "ready" &&
    Number.isFinite(cost.totalUSD) &&
    cost.pricedSamples > 0 &&
    !cost.refreshing &&
    !cost.unknownModels?.length;
  return Boolean(
    ready(displayed) &&
    ready(current) &&
    displayed.rootId === current.rootId &&
    displayed.generation === current.generation &&
    displayed.totalUSD === current.totalUSD &&
    displayed.pricedSamples === current.pricedSamples &&
    analytics.timeline?.some((row) => row.turnId === finalTurnId) &&
    (analytics.usageGeneration == null ||
      analytics.usageGeneration === current.generation),
  );
}

/** Match known argv suffixes instead of splitting paths containing spaces. */
export function bridgeProcessMatches(
  processes,
  bridges,
  root,
  serverPid,
  supervisor,
) {
  const parents = new Map(processes.map((row) => [row.pid, row.ppid]));
  const owned = (row) => {
    const seen = new Set();
    let parent = row.ppid;
    while (parent > 1 && !seen.has(parent)) {
      if (parent === serverPid) return true;
      seen.add(parent);
      parent = parents.get(parent);
    }
    return false;
  };
  return processes.flatMap((row) => {
    const handle =
      supervisor?.enabled && supervisor.health?.stateDir === supervisor.stateDir
        ? supervisor.health.handles?.find(
            (handle) =>
              handle.id === `account:${supervisor.accountKey}` &&
              handle.pid === row.pid &&
              handle.signature &&
              handle.startTime === row.startTime &&
              handle.startTime &&
              handle.stdoutReaderAlive !== false,
          )
        : null;
    if (supervisor?.enabled ? !handle : !owned(row)) return [];
    for (const bridge of bridges) {
      const suffix = ` ${bridge} ${root}`;
      if (
        row.command.endsWith(suffix) &&
        path.basename(row.command.slice(0, -suffix.length)) === "node"
      )
        return [
          {
            ...row,
            bridge,
            accountRoot: root,
            ownership: handle
              ? { kind: "supervisor", handle }
              : { kind: "backend", pid: serverPid },
          },
        ];
    }
    return [];
  });
}

export function validateSteer(
  item,
  receipt,
  submitted,
  longTurnId,
  startedAt,
  endedAt,
) {
  const input = item?.inputs?.find((input) => input.id === submitted?.id);
  if (!input || receipt?.id !== submitted?.id)
    throw new Error("Steer identity does not match the submitted message");
  if (
    receipt.status !== "delivered" ||
    !receipt.materialized ||
    input.deliveryStatus !== "delivered" ||
    input.pending ||
    !input.materialized ||
    input.requestedDelivery !== "current_turn"
  )
    throw new Error("Steer delivery is not materialized and successful");
  if (
    submitted.turnId !== longTurnId ||
    item.at < startedAt ||
    item.at > endedAt
  )
    throw new Error("Steer is not associated with turn 2");
  return { id: submitted.id, turnId: longTurnId, receipt, input, at: item.at };
}

/** Persist a complete sibling, then replace the checkpoint in one rename. */
export function atomicWriteJson(file, value, io = fs) {
  const temporary = `${file}.${process.pid}.tmp`;
  try {
    io.writeFileSync(temporary, JSON.stringify(value, null, 2) + "\n", {
      mode: 0o600,
      flag: "wx",
    });
    io.renameSync(temporary, file);
  } finally {
    if (io.existsSync(temporary)) io.unlinkSync(temporary);
  }
}

export function preserveProvenance(evidence, server, recovery = false) {
  if (recovery) {
    evidence.recoveryObservations ??= [];
    evidence.recoveryObservations.push({
      at: new Date().toISOString(),
      server,
    });
  } else evidence.server ??= server;
}

/** Canonical identity of the exact directory created by this smoke. */
export function directoryIdentity(folder) {
  const realpath = fs.realpathSync(folder);
  const stat = fs.statSync(realpath);
  if (!stat.isDirectory())
    throw new Error("Run folder identity is not a directory");
  return { realpath, dev: String(stat.dev), ino: String(stat.ino) };
}

export function verifiedPermissions(record, provider, root, expected) {
  const same = (left, right) =>
    left &&
    right &&
    left.realpath === right.realpath &&
    left.dev === right.dev &&
    left.ino === right.ino;
  const identity = directoryIdentity(root);
  if (
    !same(identity, expected) ||
    !same(directoryIdentity(record.cwd), expected)
  )
    throw new Error("Run folder identity changed");
  if (record.yoloMode !== false || record.role !== "orchestrator")
    throw new Error("Unsafe usage permissions: expected a non-YOLO lead");
  if (record.approvalPolicy != null && record.approvalPolicy !== "on-request")
    throw new Error("Unsafe usage approval policy");
  const supplied = [record.sandboxPolicy, record.sandbox].filter(
    (policy) => policy != null,
  );
  const roots = [record.cwd];
  for (const policy of supplied) {
    if (typeof policy === "string") {
      if (policy !== "workspace-write")
        throw new Error("Unsafe or unknown sandbox policy");
    } else if (
      typeof policy === "object" &&
      !Array.isArray(policy) &&
      policy.type === "workspaceWrite" &&
      (policy.networkAccess == null || policy.networkAccess === false)
    ) {
      if (policy.writableRoots != null && !Array.isArray(policy.writableRoots))
        throw new Error("Invalid writable roots policy");
      roots.push(...(policy.writableRoots ?? []));
    } else throw new Error("Unsafe or unknown sandbox policy");
  }
  if (
    roots.some(
      (folder) =>
        typeof folder !== "string" ||
        !same(directoryIdentity(folder), expected),
    )
  )
    throw new Error("Unsafe usage permissions write outside the run folder");
  const permissionMode = record.claudeOptions?.permissionMode ?? "default";
  if (provider === "claude" && permissionMode !== "default")
    throw new Error("Claude usage permissions must use default mode");
  return {
    verified: true,
    verifiedAt: new Date().toISOString(),
    source: "backend agent record; hidden policy fields via sqlite-ro",
    yoloMode: false,
    directoryIdentity: identity,
    cwd: identity.realpath,
    role: record.role,
    approvalPolicy: "on-request",
    sandbox: {
      type: "workspaceWrite",
      writableRoots: [identity.realpath],
      networkAccess: false,
    },
    rawSandbox: record.sandbox ?? null,
    rawSandboxPolicy: record.sandboxPolicy ?? null,
    policyDerivedForIdleThread: supplied.length === 0,
    ...(provider === "claude" ? { permissionMode } : {}),
    policyOwner:
      "runtime-services/src/thread_prepare.rs and claude-bridge/bridge.mjs permissionMode",
  };
}

/** Same native start identity as runtime-process/src/supervisor_identity.rs. */
export function processStartIdentity(pid) {
  if (process.platform === "linux") {
    const raw = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
    const fields = raw
      .slice(raw.lastIndexOf(")") + 1)
      .trim()
      .split(/\s+/);
    const boot = fs
      .readFileSync("/proc/sys/kernel/random/boot_id", "utf8")
      .trim();
    if (!/^\d+$/.test(fields[19] ?? "") || fields[0] === "Z")
      throw new Error("Cannot verify process start identity");
    return `linux:${boot}:${fields[19]}`;
  }
  if (process.platform !== "darwin")
    throw new Error("Native process identity unsupported");
  const code = `import ctypes,subprocess,sys
class BsdInfo(ctypes.Structure):
 _fields_=[('prefix',ctypes.c_uint32*12),('comm',ctypes.c_char*16),('name',ctypes.c_char*32),('rest',ctypes.c_uint32*6),('seconds',ctypes.c_uint64),('micros',ctypes.c_uint64)]
pid=int(sys.argv[1]); info=BsdInfo(); lib=ctypes.CDLL('/usr/lib/libproc.dylib',use_errno=True)
# PROC_PIDTBSDINFO from the macOS proc_info.h ABI.
flavor=3
lib.proc_pidinfo.argtypes=[ctypes.c_int,ctypes.c_int,ctypes.c_uint64,ctypes.c_void_p,ctypes.c_int]
lib.proc_pidinfo.restype=ctypes.c_int
read=lib.proc_pidinfo(pid,flavor,0,ctypes.byref(info),ctypes.sizeof(info))
if read!=ctypes.sizeof(info) or info.prefix[3]!=pid: raise RuntimeError('Cannot verify native process identity')
boot=subprocess.check_output(['/usr/sbin/sysctl','-n','kern.bootsessionuuid'],text=True,timeout=2).strip()
print(f'macos:{boot}:{info.seconds}.{info.micros:06d}')`;
  return execFileSync("python3", ["-c", code, String(pid)], {
    encoding: "utf8",
    timeout: 5000,
  }).trim();
}

/** Exact owned rows only; analytics fallback requires an explicit recovery flag. */
export function sqliteEvidence(state, agent, runId, includeAnalytics = false) {
  const code = `import sqlite3,json,pathlib,sys
root,agent,run,analytics=sys.argv[1:]
def db(name):
 c=sqlite3.connect(pathlib.Path(root,name+'.sqlite3').as_uri()+'?mode=ro',uri=True,timeout=0.25)
 c.execute('PRAGMA query_only=ON'); c.execute('BEGIN'); return c
c=db('canvas')
r=c.execute('SELECT record FROM runtime_agents WHERE id=? LIMIT 1',(agent,)).fetchone()
if not r: raise RuntimeError('Missing owned agent')
a=json.loads(r[0])
if not a.get('name','').startswith('Studio smoke '+run+' '): raise RuntimeError('Agent is outside the evidence run')
permissions={k:a.get(k) for k in ['yoloMode','cwd','role','sandbox','sandboxPolicy','approvalPolicy','claudeOptions']}
result={'permissions':permissions}
if analytics=='1':
 if a.get('provider')!='claude' or a.get('rootId',agent)!=agent: raise RuntimeError('SQLite analytics fallback is limited to the owned Claude lead')
 d=db('analytics')
 rows=d.execute("SELECT record FROM analytics_usage WHERE agent=? AND NULLIF(json_extract(record,'$.responseId'),'') IS NOT NULL ORDER BY at DESC,seq DESC LIMIT 129",(agent,)).fetchall()
 history=d.execute('SELECT record FROM analytics_history WHERE agent=? LIMIT 9',(agent,)).fetchall()
 if len(rows)>128 or len(history)>8: raise RuntimeError('Owned evidence exceeds the bounded snapshot')
 generation=d.execute('SELECT generation FROM analytics_usage_roots WHERE root=? LIMIT 1',(a.get('rootId',agent),)).fetchone()
 result['analytics']={'source':'sqlite-ro','agents':[a],'timeline':[json.loads(r[0]) for r in rows],'history':[json.loads(r[0]) for r in history],'usageGeneration':generation[0] if generation else 0}
print(json.dumps(result))`;
  return JSON.parse(
    execFileSync(
      "python3",
      ["-c", code, state, agent, runId, includeAnalytics ? "1" : "0"],
      { encoding: "utf8", timeout: 10000, maxBuffer: 2097152 },
    ),
  );
}
