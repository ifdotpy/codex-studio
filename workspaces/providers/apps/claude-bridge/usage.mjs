// @ts-check
/** @typedef {import('@anthropic-ai/claude-agent-sdk').SDKPartialAssistantMessage} StreamMessage */
/** @typedef {import('@anthropic-ai/claude-agent-sdk').SDKAssistantMessage} AssistantMessage */
/** @typedef {Extract<StreamEvent, {type: 'message_delta'}>['delta']['stop_reason']} StopReason */
/** @typedef {import('@anthropic-ai/claude-agent-sdk').SDKResultMessage} ResultMessage */
/** @typedef {import('@anthropic-ai/claude-agent-sdk').ModelUsage} ModelUsage */
/** @typedef {StreamMessage['event']} StreamEvent */
/** @typedef {Extract<StreamEvent, {type: 'message_start'}>['message']} Message */
/** @typedef {Extract<StreamEvent, {type: 'message_delta'}>['usage']} DeltaUsage */
/** @typedef {import('./usage-contract.generated.d.mts').UsageTokens} UsageTokens */
/** @typedef {import('./usage-contract.generated.d.mts').UsageObservation} UsageObservation */
/** @typedef {Extract<UsageObservation, {kind: 'gap'}>['reason']} GapReason */

/** Claude input excludes cache; thinking is already included in output.
 * @param {Message['usage']} usage
 * @returns {UsageTokens}
 */
export function responseTokens(usage) {
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

/** @param {ModelUsage} usage @returns {UsageTokens} */
export function modelTokens(usage) {
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

/** Every present cumulative field replaces the earlier value; null means unknown.
 * @param {Message['usage']} initial
 * @param {DeltaUsage} delta
 * @returns {Message['usage']}
 */
function mergeUsage(initial, delta) {
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
  /** @type {Map<StreamMessage['parent_tool_use_id'], {message: Message, stopReason: StopReason, turnId: string | undefined}>} */
  pending = new Map();
  /** @type {Map<string, string>} */
  parentTurns = new Map();
  /** @type {Map<string, number>} */
  modelWindows = new Map();
  // Pricing aliases are candidates, never identities for exact model windows.
  /** @type {Map<string, Map<string, number>>} */
  aliasWindows = new Map();
  // Query-scoped identities survive pending-stream removal and turn changes.
  /** @type {Set<string>} */
  started = new Set();
  /** @type {Set<string>} */
  gapped = new Set();
  closed = false;

  /** @param {string | null | undefined} parent @param {string | undefined} turnId */
  turnFor(parent, turnId) {
    return parent == null ? turnId : (this.parentTurns.get(parent) ?? turnId);
  }

  /** @param {string} toolId @param {string | undefined} turnId */
  registerTool(toolId, turnId) {
    if (turnId !== undefined && !this.parentTurns.has(toolId))
      this.parentTurns.set(toolId, turnId);
  }

  /** @param {ResultMessage['modelUsage']} models */
  recordModels(models) {
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

  /** @param {Message['model']} model */
  contextWindowFor(model) {
    const exact = this.modelWindows.get(model);
    if (exact !== undefined) return exact;
    const candidates = new Set(this.aliasWindows.get(model)?.values());
    return candidates.size === 1 ? candidates.values().next().value : undefined;
  }

  /** @param {StreamMessage} frame @param {string | undefined} turnId
   * @returns {UsageObservation[]}
   */
  observe(frame, turnId) {
    if (this.closed) return [];
    const parent = frame.parent_tool_use_id ?? null;
    const event = frame.event;
    /** @type {UsageObservation[]} */
    const observations = [];
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
   * The SDK exposes no general typed marker proving local-only assistant messages.
   * @param {AssistantMessage} frame @param {string | undefined} turnId
   * @returns {UsageObservation[]}
   */
  observeAssistant(frame, turnId) {
    const { id, model } = frame.message;
    if (this.closed || this.started.has(id)) return [];
    const ownerTurn = this.turnFor(frame.parent_tool_use_id, turnId);
    return this.gap({ message: { id, model }, turnId: ownerTurn }, "noStream");
  }

  /** @param {{message:Pick<Message, 'id' | 'model'>,turnId:string | undefined}} current
   * @param {GapReason} reason
   * @returns {UsageObservation[]}
   */
  gap(current, reason) {
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

  /** @param {GapReason} reason
   * @param {string | undefined} [turnId]
   * @returns {UsageObservation[]}
   */
  finish(reason, turnId) {
    if (reason === "queryClosed") this.closed = true;
    /** @type {UsageObservation[]} */
    const observations = [];
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

/** Query-wide totals include subagents and auxiliary pipeline calls.
 * @param {Pick<ResultMessage, 'modelUsage' | 'is_error'>} result @param {string | undefined} turnId
 * @returns {UsageObservation | null}
 */
export function cumulativeUsage(result, turnId) {
  const models = Object.entries(result.modelUsage ?? {}).map(
    ([model, usage]) => ({
      model,
      tokens: modelTokens(usage),
      providerCostUsd: usage.costUSD,
    }),
  );
  /** @type {UsageTokens} */
  const total = {
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
