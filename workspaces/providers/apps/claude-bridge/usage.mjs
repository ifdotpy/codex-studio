// @ts-check
/** @typedef {import('@anthropic-ai/claude-agent-sdk').SDKPartialAssistantMessage} StreamMessage */
/** @typedef {import('@anthropic-ai/claude-agent-sdk').SDKResultMessage} ResultMessage */
/** @typedef {import('@anthropic-ai/claude-agent-sdk').ModelUsage} ModelUsage */
/** @typedef {StreamMessage['event']} StreamEvent */
/** @typedef {Extract<StreamEvent, {type: 'message_start'}>['message']} Message */
/** @typedef {Extract<StreamEvent, {type: 'message_delta'}>['usage']} DeltaUsage */
/** @typedef {import('./usage-contract.generated.d.mts').UsageTokens} UsageTokens */
/** @typedef {import('./usage-contract.generated.d.mts').UsageObservation} UsageObservation */

/** Claude input excludes cache; thinking is already included in output.
 * @param {Message['usage']} usage
 * @returns {UsageTokens}
 */
export function responseTokens(usage) {
  return {
    uncachedInput: usage.input_tokens,
    cacheReadInput: usage.cache_read_input_tokens ?? 0,
    cacheWriteInput: usage.cache_creation_input_tokens ?? 0,
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
  /** @type {Map<StreamMessage['parent_tool_use_id'], {message: Message, turnId: string | undefined}>} */
  pending = new Map();

  /** @param {StreamMessage} frame @param {string | undefined} turnId
   * @returns {UsageObservation[]}
   */
  observe(frame, turnId) {
    const parent = frame.parent_tool_use_id ?? null;
    const event = frame.event;
    /** @type {UsageObservation[]} */
    const observations = [];
    if (event.type === "message_start") {
      const previous = this.pending.get(parent);
      if (previous) observations.push(this.gap(previous, "streamInterrupted"));
      this.pending.set(parent, {
        message: structuredClone(event.message),
        turnId,
      });
    } else {
      const current = this.pending.get(parent);
      if (!current) return observations;
      if (event.type === "message_delta")
        current.message.usage = mergeUsage(current.message.usage, event.usage);
      if (event.type === "message_stop") {
        this.pending.delete(parent);
        observations.push({
          kind: "response",
          responseId: current.message.id,
          model: current.message.model,
          tokens: responseTokens(current.message.usage),
          evidence: "live",
          ...(current.turnId === undefined ? {} : { turnId: current.turnId }),
          ...(parent === null ? {} : { parentToolUseId: parent }),
        });
      }
    }
    return observations;
  }

  /** @param {{message:Message,turnId:string | undefined}} current
   * @param {'streamInterrupted' | 'queryClosed'} reason
   * @returns {UsageObservation}
   */
  gap(current, reason) {
    return {
      kind: "gap",
      responseId: current.message.id,
      model: current.message.model,
      reason,
      ...(current.turnId === undefined ? {} : { turnId: current.turnId }),
    };
  }

  /** @param {'streamInterrupted' | 'queryClosed'} reason
   * @param {string | undefined} [turnId]
   * @returns {UsageObservation[]}
   */
  finish(reason, turnId) {
    /** @type {UsageObservation[]} */
    const observations = [];
    for (const [parent, current] of this.pending) {
      if (turnId !== undefined && current.turnId !== turnId) continue;
      observations.push(this.gap(current, reason));
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
