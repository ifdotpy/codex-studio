import type {
  SDKPartialAssistantMessage as StreamMessage,
  SDKAssistantMessage as AssistantMessage,
  SDKResultMessage as ResultMessage,
  ModelUsage,
} from "@anthropic-ai/claude-agent-sdk";
import type {
  UsageTokens,
  UsageObservation,
  UsageGapReason as GapReason,
} from "./usage-contract.generated.ts";
type StopReason = Extract<
  StreamEvent,
  { type: "message_delta" }
>["delta"]["stop_reason"];
type StreamEvent = StreamMessage["event"];
type Message = Extract<StreamEvent, { type: "message_start" }>["message"];
type DeltaUsage = Extract<StreamEvent, { type: "message_delta" }>["usage"];

/** Claude input excludes cache; thinking is already included in output. */
export function responseTokens(usage: Message["usage"]): UsageTokens {
  return {
    uncachedInput: usage.input_tokens,
    cacheReadInput: usage.cache_read_input_tokens ?? 0,
    cacheWriteInput: usage.cache_creation_input_tokens ?? 0,
    cacheWriteInput1h: Math.min(
      usage.cache_creation?.ephemeral_1h_input_tokens ?? 0,
      usage.cache_creation_input_tokens ?? 0,
    ),
    output: usage.output_tokens,
    ...(usage.output_tokens_details == null
      ? {}
      : { reasoningOutput: usage.output_tokens_details.thinking_tokens }),
  };
}

export function modelTokens(usage: ModelUsage): UsageTokens {
  return {
    uncachedInput: usage.inputTokens,
    cacheReadInput: usage.cacheReadInputTokens,
    cacheWriteInput: usage.cacheCreationInputTokens,
    output: usage.outputTokens,
    ...(usage.thinkingTokens === undefined
      ? {}
      : { reasoningOutput: usage.thinkingTokens }),
  };
}

/** Every present cumulative field replaces the earlier value; null means unknown. */
function mergeUsage(
  initial: Message["usage"],
  delta: DeltaUsage,
): Message["usage"] {
  return {
    ...initial,
    ...delta,
    input_tokens: delta.input_tokens ?? initial.input_tokens,
    cache_read_input_tokens:
      delta.cache_read_input_tokens ?? initial.cache_read_input_tokens,
    cache_creation_input_tokens:
      delta.cache_creation_input_tokens ?? initial.cache_creation_input_tokens,
    output_tokens_details:
      delta.output_tokens_details ?? initial.output_tokens_details,
  };
}

/** One stream per parent tool (null is the main loop). SDK deltas carry no response id. */
export class ResponseUsage {
  pending: Map<
    StreamMessage["parent_tool_use_id"],
    { message: Message; stopReason: StopReason; turnId: string | undefined }
  > = new Map();

  parentTurns: Map<string, string> = new Map();

  modelWindows: Map<string, number> = new Map();
  // Pricing aliases are candidates, never identities for exact model windows.

  aliasWindows: Map<string, Map<string, number>> = new Map();
  // Query-scoped identities survive pending-stream removal and turn changes.

  started: Set<string> = new Set();

  gapped: Set<string> = new Set();
  closed = false;

  turnFor(parent: string | null | undefined, turnId: string | undefined) {
    return parent == null ? turnId : (this.parentTurns.get(parent) ?? turnId);
  }

  registerTool(toolId: string, turnId: string | undefined) {
    if (turnId !== undefined && !this.parentTurns.has(toolId))
      this.parentTurns.set(toolId, turnId);
  }

  recordModels(models: ResultMessage["modelUsage"]) {
    for (const [model, usage] of Object.entries(models ?? {})) {
      if (usage.contextWindow > 0) {
        this.modelWindows.set(model, usage.contextWindow);
        if (usage.canonicalModel) {
          let candidates = this.aliasWindows.get(usage.canonicalModel);
          if (!candidates) {
            candidates = new Map();
            this.aliasWindows.set(usage.canonicalModel, candidates);
          }
          candidates.set(model, usage.contextWindow);
        }
      }
    }
  }

  contextWindowFor(model: Message["model"]) {
    const exact = this.modelWindows.get(model);
    if (exact !== undefined) return exact;
    const candidates = new Set(this.aliasWindows.get(model)?.values());
    return candidates.size === 1 ? candidates.values().next().value : undefined;
  }

  observe(
    frame: StreamMessage,
    turnId: string | undefined,
  ): UsageObservation[] {
    if (this.closed) return [];
    const parent = frame.parent_tool_use_id ?? null;
    const event = frame.event;

    const observations: UsageObservation[] = [];
    if (event.type === "message_start") {
      const previous = this.pending.get(parent);
      if (previous)
        observations.push(...this.gap(previous, "streamInterrupted"));
      if (parent !== null) this.registerTool(parent, turnId);
      this.started.add(event.message.id);
      this.pending.set(parent, {
        stopReason: null,
        message: structuredClone(event.message),
        turnId: this.turnFor(parent, turnId),
      });
    } else {
      const current = this.pending.get(parent);
      if (!current) return observations;
      if (event.type === "message_delta") {
        current.message.usage = mergeUsage(current.message.usage, event.usage);
        // CLI aborts can synthesize a null delta followed by message_stop.
        if (event.delta.stop_reason != null)
          current.stopReason = event.delta.stop_reason;
      }
      if (event.type === "message_stop") {
        this.pending.delete(parent);
        if (current.stopReason == null) {
          observations.push(...this.gap(current, "unconfirmedStop"));
          return observations;
        }
        const modelContextWindow = this.contextWindowFor(current.message.model);
        observations.push({
          kind: "response",
          responseId: current.message.id,
          model: current.message.model,
          tokens: responseTokens(current.message.usage),
          evidence: "live",
          ...(modelContextWindow === undefined ? {} : { modelContextWindow }),
          ...(current.turnId === undefined ? {} : { turnId: current.turnId }),
          ...(parent === null ? {} : { parentToolUseId: parent }),
        });
      }
    }
    return observations;
  }

  /** Assistant usage is provisional even when no stream was delivered.
   * The SDK exposes no general typed marker proving local-only assistant messages. */
  observeAssistant(
    frame: AssistantMessage,
    turnId: string | undefined,
  ): UsageObservation[] {
    const { id, model } = frame.message;
    if (this.closed || this.started.has(id)) return [];
    const ownerTurn = this.turnFor(frame.parent_tool_use_id, turnId);
    return this.gap({ message: { id, model }, turnId: ownerTurn }, "noStream");
  }

  gap(
    current: {
      message: Pick<Message, "id" | "model">;
      turnId: string | undefined;
    },
    reason: GapReason,
  ): UsageObservation[] {
    const { id, model } = current.message;
    // All gap paths share identity: keep the first diagnostic even if a later
    // stream for this response also ends unfinished. Final responses remain allowed.
    if (this.gapped.has(id)) return [];
    this.gapped.add(id);
    return [
      {
        kind: "gap",
        responseId: id,
        model,
        reason,
        ...(current.turnId === undefined ? {} : { turnId: current.turnId }),
      },
    ];
  }

  finish(reason: GapReason, turnId?: string): UsageObservation[] {
    if (reason === "queryClosed") this.closed = true;

    const observations: UsageObservation[] = [];
    for (const [parent, current] of this.pending) {
      // Background subagents may continue beyond the main Studio turn.
      if (reason === "streamInterrupted" && parent !== null) continue;
      if (turnId !== undefined && current.turnId !== turnId) continue;
      observations.push(...this.gap(current, reason));
      this.pending.delete(parent);
    }
    return observations;
  }
}

/** Query-wide totals include subagents and auxiliary pipeline calls. */
export function cumulativeUsage(
  result: Pick<ResultMessage, "modelUsage" | "is_error">,
  turnId: string | undefined,
): UsageObservation | null {
  const models = Object.entries(result.modelUsage ?? {}).map(
    ([model, usage]) => ({
      model,
      tokens: modelTokens(usage),
      providerCostUsd: usage.costUSD,
    }),
  );

  const total: UsageTokens = {
    uncachedInput: 0,
    cacheReadInput: 0,
    cacheWriteInput: 0,
    output: 0,
  };
  for (const { tokens } of models) {
    total.uncachedInput += tokens.uncachedInput;
    total.cacheReadInput += tokens.cacheReadInput;
    total.cacheWriteInput += tokens.cacheWriteInput;
    total.output += tokens.output;
  }
  // A partial thinking count must not masquerade as the query-wide count.
  if (
    models.length &&
    models.every(({ tokens }) => tokens.reasoningOutput !== undefined)
  )
    total.reasoningOutput = models.reduce(
      (sum, { tokens }) => sum + (tokens.reasoningOutput ?? 0),
      0,
    );
  if (result.is_error && Object.values(total).every((value) => value === 0))
    return null;
  const window = Math.max(
    0,
    ...Object.values(result.modelUsage ?? {}).map(
      (usage) => usage.contextWindow,
    ),
  );
  return {
    kind: "cumulative",
    scope: "claudeQuery",
    total,
    models,
    ...(turnId === undefined ? {} : { turnId }),
    ...(window > 0 ? { modelContextWindow: window } : {}),
  };
}
