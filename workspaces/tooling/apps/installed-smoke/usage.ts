/** Read-only native usage evidence and OLD/NEW presentation for installed smoke. */
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { isDeepStrictEqual } from "node:util";
import { execFileSync } from "node:child_process";
import {
  ProviderKind,
  isProviderKind,
} from "../../../client/apps/web/src/generated/providerKind.ts";

import type { Session } from "../../../providers/apps/claude-bridge/session-state.ts";
import type { components } from "../../../client/apps/web/src/generated/api.ts";
import type {
  PricingArtifact,
  RawCost,
} from "../../../client/apps/web/src/generated/pricing.ts";

/** A sparse saved view permits explicit clearing as well as absent properties. */
export type Optional<T> = { [K in keyof T]?: T[K] | undefined };
export type Schema = components["schemas"];
export type Tokens = {
  [K in keyof Schema["UsageTokens"]]: Schema["UsageTokens"][K] | null;
};
export type TokenRow = { tokens: Optional<Tokens> };
export type Transcript = Optional<
  Pick<
    Schema["TranscriptMessageRecord"],
    "id" | "role" | "text" | "at" | "turnId" | "toolStatus" | "clientMessageId"
  >
> & {
  inputs?: Optional<Schema["TranscriptInput"]>[] | null | undefined;
};
export type LegacyUsageRow = Optional<
  Pick<
    Schema["AnalyticsUsageRecord"],
    | "responseId"
    | "turnId"
    | "model"
    | "counterDomain"
    | "delta"
    | "requestUsage"
    | "last"
    | "inputTokensAreUncached"
    | "usageSource"
    | "studioUsage"
  >
>;
export type Analytics = Omit<
  Optional<Schema["AnalyticsResponse"]>,
  "timeline"
> & {
  timeline?: LegacyUsageRow[] | null | undefined;
  source?: string;
  usageGeneration?: Schema["JsonValue"];
};
export type Cost = Omit<Optional<Schema["SessionCostResponse"]>, "rootId"> &
  Optional<Pick<Schema["AgentSummaryEntityDto"], "rootId">>;
export type ProcessRow = {
  pid: number;
  ppid: number;
  command: string;
  startTime?: string | null;
};
export type Supervisor = {
  enabled: boolean;
  stateDir: string;
  accountKey: string;
  health?: {
    stateDir?: string;
    handles?: Optional<Schema["SupervisorHandle"]>[];
  } | null;
};
export type DirectoryIdentity = ReturnType<typeof directoryIdentity>;
export type Timing = {
  route: string;
  startedAt: string;
  timeoutMs: number;
  outcome?: "success" | "timeout" | "error";
  elapsedMs?: number;
  agentId?: string;
};

export type Response = {
  responseId?: string | null | undefined;
  turnId?: string | null | undefined;
  model?: string | null | undefined;
  tokens: Optional<Tokens>;
  rawUsage?: unknown;
};
export type ServerObservation = {
  binary?: string;
  sha256?: string;
  stateDir?: string | null;
  pid?: number;
  release?: string;
};
export type BridgeObservation = Optional<ProcessRow> & {
  bridge: string;
  sha256: string;
  observedAt?: string;
  release?: string;
};
export type Steer = {
  id?: string | undefined;
  turnId?: string | null | undefined;
  sentAt?: string;
  receipt?: Schema["studio_api__work__models__MessageReceipt"];
  association?: string;
};
export type SavedProvider = {
  provider: ProviderKind;
  agentId: string;
  truth: {
    tokens: Optional<Tokens>;
    responses?: Response[];
    prices?: { responseId?: string | null | undefined; rates: RawCost }[];
    costUsd?: number;
    recomputedCostUsd?: number | null;
  };
  recorded: {
    tokens: Optional<Tokens>;
    responses?: Response[];
    gaps?: unknown[];
    projection?: string;
    recomputedCostUsd?: number | null;
  };
  budget: {
    tokensUsed?: Schema["AgentDetailEntityDto"]["tokensUsed"] | undefined;
    tokenUsageAccounting?:
      | Schema["AnalyticsAgentRecord"]["tokenUsageAccounting"]
      | undefined;
  };
  analytics?: Analytics;
  displayedCost?: Cost | null;
  reconciliation?: Optional<Schema["UsageReconciliationResponse"]> | null;
  analyticsLatency?: Timing[];
  bridgeProcesses?: BridgeObservation[];
  permissions?: unknown;
  publication?: {
    currentCost: Cost;
    finalTurnId?: string | null | undefined;
    capturedAt?: string;
  };
  scenario?: ReturnType<typeof scenarioEvidence>;
};
export type Evidence = {
  format?: string;
  runId?: string | null;
  startedAt?: string;
  providers: SavedProvider[];
  server?: ServerObservation;
  recoveryObservations?: { at: string; server: ServerObservation }[];
  pricingBasis?: string;
  analyticsLatency?: Timing[];
  submissions?: {
    route: string;
    id: string;
    room?: string | undefined;
    at: string;
  }[];
  chatIds?: string[];
  runFolder?: string | null;
  rootIdentity?: DirectoryIdentity | undefined;
  updatedAt?: string;
  recoveredAt?: string;
  checks?: import("./smoke.ts").ReportRow[] | undefined;
  servingBridges?: Record<string, BridgeObservation[]>;
  permissions?: Record<string, unknown>;
  scenarios?: Record<string, { steer: Steer; inputId: string }>;
};

/** Saved checkpoints are written by this smoke; optional fields cover older versions. */
export function readEvidence(file: string): Evidence {
  const evidence: Evidence = JSON.parse(fs.readFileSync(file, "utf8"));
  for (const row of evidence.providers)
    if (!isProviderKind(row.provider))
      throw new Error(`Unsupported provider for saved agent ${row.agentId}`);
  return evidence;
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** models.dev namespace for each application provider; these are pricing keys. */
export const PRICING_PROVIDERS = {
  [ProviderKind.Codex]: "openai",
  [ProviderKind.Claude]: "anthropic",
} as const satisfies Record<ProviderKind, string>;

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
] as const;

type ClaudeUsage = Parameters<
  typeof import("../../../providers/apps/claude-bridge/usage.ts").responseTokens
>[0];
type NativeUsageDocument = Partial<Omit<ClaudeUsage, "cache_creation">> & {
  cache_creation?: Partial<NonNullable<ClaudeUsage["cache_creation"]>> | null;
} & Partial<
    Record<
      | "inputTokens"
      | "cached_input_tokens"
      | "cachedInputTokens"
      | "cache_write_input_tokens"
      | "cacheWriteInputTokens"
      | "outputTokens"
      | "reasoning_output_tokens"
      | "reasoningOutputTokens",
      ClaudeUsage["input_tokens"]
    >
  >;
type JsonString = Extract<Schema["JsonValue"], string | null>;
type NativeJournalDocument = Partial<Record<"type", JsonString>> & {
  message?: Partial<Record<"id" | "model", JsonString>> & {
    usage?: NativeUsageDocument;
  };
  payload?: Partial<Record<"model" | "response_id" | "turn_id", JsonString>> & {
    usage?: NativeUsageDocument;
  };
};

/** Codex includes cache reads in input; Claude reports uncached input separately. */
export function nativeTokens(
  rawUsage: unknown,
  provider: ProviderKind,
): Schema["UsageTokens"] {
  if (!isRecord(rawUsage)) throw new Error("Native usage is not an object");
  // Sparse SDK usage plus legacy journal aliases; validate counts before arithmetic.
  const usage = rawUsage as NativeUsageDocument;
  const number = (value: unknown) => {
    if (typeof value !== "number")
      throw new Error("Native token count is not numeric");
    return value;
  };
  const input = number(usage.input_tokens ?? usage.inputTokens ?? 0);
  const cacheReadInput = number(
    usage.cache_read_input_tokens ??
      usage.cached_input_tokens ??
      usage.cachedInputTokens ??
      0,
  );
  const tokens = {
    cacheReadInput,
    cacheWriteInput: number(
      usage.cache_creation_input_tokens ??
        usage.cache_write_input_tokens ??
        usage.cacheWriteInputTokens ??
        0,
    ),
    output: number(usage.output_tokens ?? usage.outputTokens ?? 0),
    reasoningOutput: number(
      usage.reasoning_output_tokens ?? usage.reasoningOutputTokens ?? 0,
    ),
  };
  switch (provider) {
    case ProviderKind.Codex:
      return {
        ...tokens,
        uncachedInput:
          input -
          cacheReadInput -
          number(
            usage.cache_write_input_tokens ?? usage.cacheWriteInputTokens ?? 0,
          ),
      };
    case ProviderKind.Claude:
      return {
        ...tokens,
        uncachedInput: input,
        cacheWriteInput1h: Math.min(
          number(
            isRecord(usage.cache_creation)
              ? (usage.cache_creation.ephemeral_1h_input_tokens ?? 0)
              : 0,
          ),
          number(usage.cache_creation_input_tokens ?? 0),
        ),
      };
  }
  throw new Error(`Unsupported provider: ${provider satisfies never}`);
}

export function sumTokens(rows: readonly TokenRow[]): Tokens {
  return {
    ...Object.fromEntries(
      TOKEN_CATEGORIES.map((key) => [
        key,
        rows.some((row) => row.tokens[key] == null)
          ? null
          : rows.reduce((sum, row) => sum + row.tokens[key]!, 0),
      ]),
    ),
    // Contract v4: an absent lifetime split contributes zero one-hour writes.
    cacheWriteInput1h: rows.reduce(
      (sum, row) => sum + (row.tokens.cacheWriteInput1h ?? 0),
      0,
    ),
  } as Tokens;
}

export function totalTokens(tokens: Optional<Tokens>) {
  if (TOKEN_CATEGORIES.slice(0, 4).some((key) => tokens[key] == null))
    return null;
  return (
    tokens.uncachedInput! +
    tokens.cacheReadInput! +
    tokens.cacheWriteInput! +
    tokens.output!
  );
}

/** Final journal entry wins per Claude message id; partial content blocks repeat that id. */
export function nativeResponses(
  rawRecords: unknown[],
  provider: ProviderKind,
): Response[] {
  const records = rawRecords;
  const responses = new Map<string, Response>();
  let model: string | null | undefined;
  for (const raw of records) {
    if (!isRecord(raw)) continue;
    const row = raw as NativeJournalDocument;
    const payload = isRecord(row.payload) ? row.payload : {};
    const message = isRecord(row.message) ? row.message : {};
    if (row.type === "turn_context")
      model =
        typeof payload.model === "string" || payload.model === null
          ? payload.model
          : undefined;
    switch (provider) {
      case ProviderKind.Claude:
        if (
          row.type === "assistant" &&
          typeof message.id === "string" &&
          message.id &&
          message.usage &&
          message.model !== "<synthetic>"
        ) {
          responses.set(message.id, {
            responseId: message.id,
            model:
              typeof message.model === "string" || message.model === null
                ? message.model
                : undefined,
            tokens: nativeTokens(message.usage, provider),
            rawUsage: message.usage,
          });
        }
        continue;
      case ProviderKind.Codex:
        if (
          row.type === "token_usage_record" &&
          typeof payload.response_id === "string" &&
          payload.response_id
        ) {
          responses.set(payload.response_id, {
            responseId: payload.response_id,
            turnId:
              typeof payload.turn_id === "string" || payload.turn_id === null
                ? payload.turn_id
                : undefined,
            model: typeof payload.model === "string" ? payload.model : model,
            tokens: nativeTokens(payload.usage, provider),
            rawUsage: payload.usage,
          });
        }
        continue;
    }
    throw new Error(`Unsupported provider: ${provider satisfies never}`);
  }
  return [...responses.values()];
}

function jsonLines(file: string): unknown[] {
  return fs
    .readFileSync(file, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

/** Discover only this thread's files. Never scan or copy other chats' contents. */
function nativeFiles(
  state: string,
  agent: Optional<
    Pick<Schema["AnalyticsAgentRecord"], "id" | "provider" | "threadId">
  >,
  account: Pick<Schema["Account"], "id" | "home" | "claudeOptions">,
  history: Schema["AnalyticsHistoryRecord"][],
  provider: ProviderKind,
) {
  const files = new Set<string>();
  switch (provider) {
    case ProviderKind.Claude: {
      const serverRoot =
        account.id === "default"
          ? state
          : path.join(state, "account-servers", account.id);
      const snapshot = path.join(
        serverRoot,
        "sessions",
        `${agent.threadId}.json`,
      );
      const saved: unknown = JSON.parse(fs.readFileSync(snapshot, "utf8"));
      if (!isRecord(saved))
        throw new Error("Native session snapshot is not an object");
      const document = saved.session ?? saved;
      if (!isRecord(document))
        throw new Error("Native session is not an object");
      const session = document as Partial<Session>;
      const ids = new Set<unknown>([agent.threadId, session.nativeId]);
      if (Array.isArray(session.historyBranches))
        for (const branch of session.historyBranches)
          if (isRecord(branch)) ids.add(branch.nativeId);
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
          if (typeof id !== "string" || !id || path.basename(id) !== id)
            continue;
          const file = path.join(projects, project.name, `${id}.jsonl`);
          if (fs.existsSync(file)) files.add(file);
        }
      }
      return files;
    }
    case ProviderKind.Codex: {
      for (const entry of history)
        if (
          entry.threadId === agent.threadId &&
          typeof entry.path === "string" &&
          entry.path
        )
          files.add(entry.path);
      if (!files.size) {
        const sessions = path.join(account.home, "sessions");
        const walk = (folder: string, depth: number) => {
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
      return files;
    }
  }
  throw new Error(`Unsupported provider: ${provider satisfies never}`);
}

/** Discover only this thread's files. Never scan or copy other chats' contents. */
export function nativeEvidence(
  state: string,
  agent: Optional<
    Pick<Schema["AnalyticsAgentRecord"], "id" | "provider" | "threadId">
  >,
  account: Pick<Schema["Account"], "id" | "home" | "claudeOptions">,
  history: Schema["AnalyticsHistoryRecord"][],
) {
  const provider = agent.provider;
  if (!isProviderKind(provider))
    throw new Error(`Unsupported provider for agent ${agent.id}`);
  const files = nativeFiles(state, agent, account, history, provider);
  if (!files.size)
    throw new Error(
      `No native usage evidence found for ${agent.id} thread ${agent.threadId}`,
    );
  const responses = nativeResponses([...files].flatMap(jsonLines), provider);
  if (!responses.length)
    throw new Error(
      `No final native response usage found in ${[...files].join(", ")}`,
    );
  return { files: [...files], responses, tokens: sumTokens(responses) };
}

/** Canonical observations take precedence, including identity and non-response kinds. */
export function recordedUsage(analytics: Analytics) {
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
      ["nativeNotice", "codexThread", "claudeQuery"].includes(
        row.counterDomain!,
      )
    )
      continue;
    const known = (
      key: keyof Pick<
        Schema["AnalyticsProviderTokens"],
        | "inputTokens"
        | "cachedInputTokens"
        | "cacheWriteInputTokens"
        | "outputTokens"
        | "reasoningOutputTokens"
      >,
    ) => Number.isSafeInteger(selected?.[key]) && selected![key]! >= 0;
    const selected =
      row.delta?.inputTokens != null && row.delta?.outputTokens != null
        ? row.delta
        : row.requestUsage && typeof row.requestUsage === "object"
          ? row.requestUsage
          : row.last;
    const input = known("inputTokens") ? selected!.inputTokens! : null;
    const read = known("cachedInputTokens")
      ? selected!.cachedInputTokens!
      : null;
    const write = known("cacheWriteInputTokens")
      ? selected!.cacheWriteInputTokens!
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
            : input! - cacheReadInput! - cacheWriteInput!,
        cacheReadInput,
        cacheWriteInput,
        output: known("outputTokens") ? selected!.outputTokens! : null,
        reasoningOutput: known("reasoningOutputTokens")
          ? selected!.reasoningOutputTokens!
          : null,
      },
      evidence: "legacy",
      record: row,
    });
  }
  return { responses, gaps };
}

export function recordedResponses(analytics: Analytics) {
  return recordedUsage(analytics).responses;
}

export function usageTable(evidence: Evidence, label: string) {
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
        : totalTokens(row.recorded.tokens)! / totalTokens(row.truth.tokens)!,
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
      row.provider === ProviderKind.Claude
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
export function compareEvidence(oldEvidence: Evidence, newEvidence: Evidence) {
  // One common basis for both runs: OLD's saved per-response rates, joined to
  // their native models. Missing or inconsistent rates cannot produce a price.
  const modelsByProvider = new Map<ProviderKind, Map<string, RawCost | null>>();
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
  const reprice = (
    responses: Response[] | undefined,
    provider: ProviderKind,
  ) => {
    if (
      !Array.isArray(responses) ||
      responses.some((row) =>
        TOKEN_CATEGORIES.slice(0, 4).some(
          (key) =>
            !Number.isSafeInteger(row.tokens?.[key]) || row.tokens[key]! < 0,
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
              [PRICING_PROVIDERS[provider]]: { models },
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
  const project = (evidence: Evidence): Evidence => ({
    ...evidence,
    pricingBasis:
      "OLD truth.prices rates by native model; USD per million tokens; base tiers only; missing/conflicting rates or incomplete usage = null",
    providers: evidence.providers.map((row) => {
      const available = Array.isArray(row.analytics?.timeline);
      const recorded = available ? recordedUsage(row.analytics!) : null;
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
              ) as Tokens,
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
export function priceResponses(
  responses: Response[],
  catalog: Pick<PricingArtifact, "catalog">,
  provider: ProviderKind,
) {
  const models = catalog.catalog.providers[PRICING_PROVIDERS[provider]]!.models;
  const rows = responses.map((response) => {
    const cost = models[response.model!]?.cost;
    if (!cost) throw new Error(`No price for native model ${response.model}`);
    const tokens = response.tokens;
    const context =
      tokens.uncachedInput! + tokens.cacheReadInput! + tokens.cacheWriteInput!;
    const thresholds = (cost.tiers ?? [])
      .filter((tier) => tier.tier?.type === "context")
      .map((tier) => tier.tier!.size!)
      .filter(Number.isFinite);
    if (
      (cost.context_over_200k && !thresholds.length) ||
      thresholds.some((threshold) => context >= threshold!)
    )
      throw new Error("Usage smoke exceeds base-rate pricing scope");
    const hour = tokens.cacheWriteInput1h ?? 0;
    const parts: [number, number | null | undefined][] = [
      [tokens.uncachedInput!, cost.input],
      [tokens.cacheReadInput!, cost.cache_read],
      [tokens.cacheWriteInput! - hour, cost.cache_write],
      [hour, cost.input! * CLAUDE_CACHE_WRITE_1H_INPUT_MULTIPLIER],
      [tokens.output!, cost.output],
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
export function accountingEvidence(
  state: string,
  agent: string,
): string | null {
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

type ToolEvidence = Record<string, Schema["JsonValue"] | undefined> &
  Partial<
    Record<
      "command" | "cwd" | "aggregatedOutput",
      Extract<Schema["JsonValue"], string>
    >
  > & { arguments?: Record<string, Schema["JsonValue"]> };

/** Transcript proof of three turns, two long-turn responses, pwd and Claude steer. */
export function scenarioEvidence(
  items: Transcript[],
  provider: ProviderKind,
  root: string,
): {
  longTurnId: Transcript["turnId"];
  assistantResponses: number;
  words: number;
  tool:
    | (Record<string, unknown> &
        Pick<Transcript, "id" | "turnId" | "toolStatus">)
    | undefined;
  steer:
    | (Pick<Transcript, "id" | "at"> &
        Optional<ReturnType<typeof validateSteer>>)
    | null;
} {
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
  const secondItem = items[second];
  if (!secondItem || third <= second)
    throw new Error("Usage scenario is missing turn 2 or turn 3");
  const long = items.slice(second + 1, third);
  const answers = long.filter(
    (item) => item.role === "assistant" && item.text?.trim(),
  );
  const tools = long
    .filter((item) => item.role === "output")
    .map(
      (
        item,
      ): ToolEvidence & Pick<Transcript, "id" | "turnId" | "toolStatus"> => {
        const value: unknown = JSON.parse(item.text ?? "");
        if (!isRecord(value)) throw new Error("Tool evidence is not an object");
        return {
          id: item.id,
          turnId: item.turnId,
          toolStatus: item.toolStatus,
          ...(value as ToolEvidence),
        };
      },
    );
  if (answers.length < 2)
    throw new Error("Long turn has fewer than two assistant responses");
  const readOnlyNote = secondItem.text!.includes(NOTES_FILE);
  const tool = tools[0];
  const file = path.join(root, NOTES_FILE);
  const bareCat = /^[A-Za-z0-9_./-]+$/.test(file) ? `cat ${file}` : null;
  const plainCat =
    (bareCat && tool?.command === bareCat) ||
    tool?.command === `cat '${file.replaceAll("'", "'\\''")}'` ||
    (!/[\\$`"]/.test(file) && tool?.command === `cat "${file}"`);
  const shellWrapper = /^\/bin\/(?:ba|z)?sh -(?:lc|c) (.+)$/.exec(
    typeof tool?.command === "string" ? tool.command : "",
  );
  // Codex quotes the wrapped script as one argument: -lc 'cat /path'.
  const wrapped =
    /^'([^']*)'$/.exec(shellWrapper?.[1] ?? "")?.[1] ?? shellWrapper?.[1];
  const codexCat =
    provider === ProviderKind.Codex &&
    tool?.type === "commandExecution" &&
    tool.cwd === root &&
    (plainCat || (bareCat && wrapped === bareCat)) &&
    tool.exitCode === 0 &&
    typeof tool.aggregatedOutput === "string" &&
    tool.aggregatedOutput.trim() === NOTES_MARKER;
  const claudeRead =
    provider === ProviderKind.Claude &&
    tool?.tool === "Read" &&
    isRecord(tool.arguments) &&
    tool.arguments.file_path === file &&
    Object.keys(tool.arguments).every((key) =>
      ["file_path", "offset", "limit"].includes(key),
    ) &&
    JSON.stringify(tool.result ?? tool.output ?? "").includes(NOTES_MARKER);
  const validTool =
    tool !== undefined &&
    tools.length === 1 &&
    (readOnlyNote
      ? tool.toolStatus === "completed" && (codexCat || claudeRead)
      : tool.exitCode === 0 &&
        /\bpwd$/.test(typeof tool.command === "string" ? tool.command : "") &&
        tool.cwd === root &&
        typeof tool.aggregatedOutput === "string" &&
        tool.aggregatedOutput.includes(root));
  if (!validTool)
    throw new Error(
      `Long turn did not complete exactly one ${readOnlyNote ? "note read" : "pwd call"} in the run folder`,
    );
  const steer = long.find(
    (item) => item.role === "user" && item.text?.includes("usage-check steer"),
  );
  if (provider === ProviderKind.Claude && !steer)
    throw new Error("Claude steer is absent from the transcript");
  return {
    longTurnId: tool.turnId,
    assistantResponses: answers.length,
    words: answers
      .map((item) => item.text)
      .join(" ")
      .trim()
      .split(/\s+/).length,
    tool,
    steer: steer ? { id: steer.id, at: steer.at } : null,
  };
}

/** A displayed snapshot must match the fresh server computation covering final-turn usage. */
export function publicationReady(
  displayed: Cost | null | undefined,
  current: Cost | null | undefined,
  analytics: Analytics,
  finalTurnId: string | null | undefined,
): displayed is Cost {
  const ready = (cost: Cost | null | undefined): cost is Cost =>
    cost?.pricingState === "ready" &&
    Number.isFinite(cost.totalUSD) &&
    cost.pricedSamples! > 0 &&
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
  processes: ProcessRow[],
  bridges: string[],
  root: string,
  serverPid: number,
  supervisor?: Supervisor,
) {
  const parents = new Map(processes.map((row) => [row.pid, row.ppid]));
  const owned = (row: ProcessRow) => {
    const seen = new Set();
    let parent: number | undefined = row.ppid;
    while (parent! > 1 && !seen.has(parent!)) {
      if (parent === serverPid) return true;
      seen.add(parent!);
      parent = parents.get(parent!);
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
  item: Transcript | undefined,
  receipt: Optional<Schema["MessageReceiptEntityDto"]> | undefined,
  submitted:
    | { id?: string | null | undefined; turnId?: string | null | undefined }
    | undefined,
  longTurnId: string | null | undefined,
  startedAt: Transcript["at"],
  endedAt: number,
) {
  const input = item?.inputs?.find((input) => input.id === submitted?.id);
  if (!item || !submitted || !receipt || !input || receipt.id !== submitted.id)
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
    Number(item.at) < Number(startedAt) ||
    Number(item.at) > endedAt
  )
    throw new Error("Steer is not associated with turn 2");
  return {
    id: submitted.id,
    turnId: longTurnId,
    receipt,
    input,
    at: item.at,
  };
}

/** Persist a complete sibling, then replace the checkpoint in one rename. */
export function atomicWriteJson(
  file: string,
  value: unknown,
  io: Pick<
    typeof fs,
    "writeFileSync" | "renameSync" | "existsSync" | "unlinkSync"
  > = fs,
) {
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

export function preserveProvenance(
  evidence: Pick<Evidence, "server" | "recoveryObservations">,
  server: ServerObservation,
  recovery = false,
) {
  if (recovery) {
    evidence.recoveryObservations ??= [];
    evidence.recoveryObservations.push({
      at: new Date().toISOString(),
      server,
    });
  } else evidence.server ??= server;
}

/** Canonical identity of the exact directory created by this smoke. */
export function directoryIdentity(folder: string) {
  const realpath = fs.realpathSync(folder);
  const stat = fs.statSync(realpath);
  if (!stat.isDirectory())
    throw new Error("Run folder identity is not a directory");
  return { realpath, dev: String(stat.dev), ino: String(stat.ino) };
}

export type PermissionRecord = Optional<
  Pick<Schema["AgentRecordResponse"], "approvalPolicy" | "sandbox">
> &
  Partial<Pick<Schema["AgentResponse"], "cwd" | "role">> & {
    sandboxPolicy?: unknown;
    yoloMode?: Schema["AgentDetailEntityDto"]["yoloMode"] | undefined;
    claudeOptions?: Schema["AgentDetailEntityDto"]["claudeOptions"] | undefined;
  };

export function verifiedPermissions(
  record: PermissionRecord,
  provider: ProviderKind,
  root: string,
  expected: DirectoryIdentity | undefined,
) {
  const same = (
    left: DirectoryIdentity | undefined,
    right: DirectoryIdentity | undefined,
  ) =>
    left &&
    right &&
    left.realpath === right.realpath &&
    left.dev === right.dev &&
    left.ino === right.ino;
  const identity = directoryIdentity(root);
  if (
    !same(identity, expected) ||
    !same(directoryIdentity(record.cwd!), expected)
  )
    throw new Error("Run folder identity changed");
  if (record.yoloMode !== false || record.role !== "orchestrator")
    throw new Error("Unsafe usage permissions: expected a non-YOLO lead");
  if (record.approvalPolicy != null && record.approvalPolicy !== "on-request")
    throw new Error("Unsafe usage approval policy");
  const supplied = (
    [record.sandboxPolicy, record.sandbox] as (
      | string
      | Record<string, Schema["JsonValue"]>
      | null
      | undefined
    )[]
  ).filter((policy) => policy != null);
  const roots: unknown[] = [record.cwd];
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
  if (provider === ProviderKind.Claude && permissionMode !== "default")
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
    ...(provider === ProviderKind.Claude ? { permissionMode } : {}),
    policyOwner:
      "runtime-services/src/thread_prepare.rs and claude-bridge/bridge.mjs permissionMode",
  };
}

/** Same native start identity as runtime-process/src/supervisor_identity.rs. */
export function processStartIdentity(pid: number) {
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
export function sqliteEvidence(
  state: string,
  agent: string,
  runId: string,
  includeAnalytics = false,
): { permissions: PermissionRecord; analytics: Analytics } {
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
 if a.get('provider')!=${JSON.stringify(ProviderKind.Claude)} or a.get('rootId',agent)!=agent: raise RuntimeError('SQLite analytics fallback is limited to the owned Claude lead')
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
