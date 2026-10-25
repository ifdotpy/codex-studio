/** Read-only native usage evidence and OLD/NEW presentation for installed smoke. */
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { execFileSync } from "node:child_process";

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
    output: usage.output_tokens ?? usage.outputTokens ?? 0,
    reasoningOutput:
      usage.reasoning_output_tokens ?? usage.reasoningOutputTokens ?? 0,
  };
}

export function sumTokens(rows) {
  return Object.fromEntries(
    TOKEN_CATEGORIES.map((key) => [
      key,
      rows.reduce((sum, row) => sum + (row.tokens[key] ?? 0), 0),
    ]),
  );
}

export function totalTokens(tokens) {
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

export function recordedResponses(analytics, provider) {
  return (analytics.timeline ?? [])
    .filter((row) => row.responseId)
    .map((row) => ({
      responseId: row.responseId,
      turnId: row.turnId,
      model: row.model,
      tokens:
        row.studioUsage?.tokens ??
        row.tokens ??
        nativeTokens(
          row.requestUsage ?? row.delta ?? row.last,
          row.inputTokensAreUncached ? "claude" : provider,
        ),
      record: row,
    }));
}

export function usageTable(evidence, label) {
  return evidence.providers.map((row) => ({
    run: label,
    provider: row.provider,
    agent: row.agentId,
    "truth input/read/write/output/reasoning": TOKEN_CATEGORIES.map(
      (key) => row.truth.tokens[key],
    ).join("/"),
    "recorded input/read/write/output/reasoning": TOKEN_CATEGORIES.map(
      (key) => row.recorded.tokens[key],
    ).join("/"),
    "recorded/truth":
      totalTokens(row.recorded.tokens) / totalTokens(row.truth.tokens),
    "budget tokens": row.budget.tokensUsed,
    accounting: row.budget.tokenUsageAccounting,
    "truth priced USD": row.truth.costUsd ?? null,
    "displayed USD": row.displayedCost?.totalUSD ?? null,
    "reconciled recorded USD":
      row.reconciliation?.totals?.recordedCostUsd ?? null,
    "reconciled evidence USD":
      row.reconciliation?.totals?.evidenceCostUsd ?? null,
  }));
}

export function compareEvidence(oldEvidence, newEvidence) {
  return [...usageTable(oldEvidence, "OLD"), ...usageTable(newEvidence, "NEW")];
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
    const parts = [
      [tokens.uncachedInput, cost.input],
      [tokens.cacheReadInput, cost.cache_read],
      [tokens.cacheWriteInput, cost.cache_write],
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
      ...JSON.parse(item.text),
    }));
  if (answers.length < 2)
    throw new Error("Long turn has fewer than two assistant responses");
  if (
    tools.length !== 1 ||
    tools[0].exitCode !== 0 ||
    !/\bpwd$/.test(tools[0].command ?? "") ||
    tools[0].cwd !== root ||
    !tools[0].aggregatedOutput?.includes(root)
  )
    throw new Error(
      "Long turn did not complete exactly one pwd call in the run folder",
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
    analytics.timeline?.some((row) => row.turnId === finalTurnId),
  );
}

/** Match known argv suffixes instead of splitting paths containing spaces. */
export function bridgeProcessMatches(processes, bridges, root, serverPid) {
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
    if (!owned(row)) return [];
    for (const bridge of bridges) {
      const suffix = ` ${bridge} ${root}`;
      if (
        row.command.endsWith(suffix) &&
        path.basename(row.command.slice(0, -suffix.length)) === "node"
      )
        return [{ ...row, bridge, accountRoot: root }];
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
