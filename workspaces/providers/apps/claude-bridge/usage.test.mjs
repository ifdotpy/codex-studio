// @ts-check
import { expect, test } from "vitest";
import {
  ResponseUsage,
  cumulativeUsage,
  modelTokens,
  responseTokens,
} from "./usage.mjs";
import {
  events,
  modelUsage,
  interruptedDelta,
  assistant,
} from "./usage-fixtures.mjs";
/** @typedef {import('@anthropic-ai/claude-agent-sdk').SDKPartialAssistantMessage} StreamMessage */
/** @typedef {import('@anthropic-ai/claude-agent-sdk').SDKResultMessage} ResultMessage */
/** @param {StreamMessage['event']} event @param {string | null} [parent] @returns {StreamMessage} */
const frame = (event, parent = null) => ({
  type: "stream_event",
  event,
  parent_tool_use_id: parent,
  uuid: "11111111-1111-4111-8111-111111111111",
  session_id: "session",
});
// The type is SDK-owned even for a deliberately partial fixture of result metadata.
/** @param {ResultMessage['modelUsage']} models @param {boolean} [is_error] @returns {Pick<ResultMessage, 'modelUsage' | 'is_error'>} */
const result = (models, is_error = false) => ({ modelUsage: models, is_error });

test("message deltas replace input, cache and output categories; stop finalizes once", () => {
  const usage = new ResponseUsage();
  /** @type {import('./usage-contract.generated.d.mts').UsageObservation[]} */
  const observations = [];
  for (const event of events)
    observations.push(...usage.observe(frame(event), "turn"));
  expect(observations).toEqual([
    {
      kind: "response",
      responseId: "response-1",
      turnId: "turn",
      model: "claude-opus-5-5",
      evidence: "live",
      tokens: {
        uncachedInput: 12,
        cacheReadInput: 32,
        cacheWriteInput: 42,
        cacheWriteInput1h: 0,
        output: 720,
        reasoningOutput: 20,
      },
    },
  ]);
  expect(usage.observe(frame({ type: "message_stop" }), "turn")).toEqual([]);
  expect(usage.finish("queryClosed")).toEqual([]);
});
test("null cumulative fields retain earlier counts and explicit zero replaces them", () => {
  const usage = new ResponseUsage();
  const start = events.find((e) => e.type === "message_start");
  const delta = events.find((e) => e.type === "message_delta");
  if (!start || !delta) throw new Error("missing SDK fixture");
  usage.observe(frame(start), "turn");
  usage.observe(
    frame({
      ...delta,
      usage: {
        ...delta.usage,
        input_tokens: null,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: null,
        output_tokens: 0,
        output_tokens_details: null,
      },
    }),
    "turn",
  );
  const observation = usage.observe(frame({ type: "message_stop" }), "turn")[0];
  expect(observation).toMatchObject({
    tokens: {
      uncachedInput: 10,
      cacheReadInput: 0,
      cacheWriteInput: 40,
      output: 0,
    },
  });
});
test("interleaved subagent streams have independent identities and unfinished gaps drain once", () => {
  const usage = new ResponseUsage();
  const start = events.find((e) => e.type === "message_start");
  if (!start) throw new Error("missing SDK fixture");
  usage.observe(frame(start), "main-turn");
  usage.observe(
    frame({ ...start, message: { ...start.message, id: "child" } }, "tool"),
    "main-turn",
  );
  const delta = events.find((e) => e.type === "message_delta");
  if (!delta) throw new Error("missing SDK fixture");
  usage.observe(frame(delta, "tool"), "main-turn");
  expect(
    usage.observe(frame({ type: "message_stop" }, "tool"), "main-turn"),
  ).toMatchObject([
    { kind: "response", responseId: "child", parentToolUseId: "tool" },
  ]);
  expect(usage.finish("streamInterrupted", "other-turn")).toEqual([]);
  expect(usage.finish("queryClosed")).toMatchObject([
    { kind: "gap", responseId: "response-1", reason: "queryClosed" },
  ]);
  expect(usage.finish("queryClosed")).toEqual([]);
});
test("thinking remains inside output, absent thinking stays unknown", () => {
  expect(modelTokens(modelUsage)).toEqual({
    uncachedInput: 10,
    cacheReadInput: 30,
    cacheWriteInput: 40,
    output: 720,
    reasoningOutput: 20,
  });
  const { thinkingTokens: _, ...unknown } = modelUsage;
  expect(modelTokens(unknown)).not.toHaveProperty("reasoningOutput");
});

test("cumulative model totals are summed without subtracting equal per-turn usage", () => {
  const input = result({
    main: modelUsage,
    child: {
      ...modelUsage,
      inputTokens: 2,
      outputTokens: 5,
      thinkingTokens: 1,
      costUSD: 0.02,
    },
  });
  const observation = cumulativeUsage(input, "turn");
  expect(observation).toMatchObject({
    kind: "cumulative",
    scope: "claudeQuery",
    total: {
      uncachedInput: 12,
      cacheReadInput: 60,
      cacheWriteInput: 80,
      output: 725,
      reasoningOutput: 21,
    },
    modelContextWindow: 200000,
  });
  expect(cumulativeUsage(input, "next-turn")).toMatchObject({
    total: observation?.kind === "cumulative" ? observation.total : undefined,
  });
});
test("zeroed error results are skipped, zero success and nonzero error totals remain observations", () => {
  expect(cumulativeUsage(result({}, true), "turn")).toBeNull();
  expect(cumulativeUsage(result({}), "turn")).toMatchObject({
    kind: "cumulative",
    total: { output: 0 },
  });
  expect(
    cumulativeUsage(result({ main: modelUsage }, true), "turn"),
  ).toMatchObject({ kind: "cumulative", total: { output: 720 } });
});
test("mixed known and unknown model thinking totals remain unknown", () => {
  const { thinkingTokens: _, ...unknown } = modelUsage;
  const observation = cumulativeUsage(
    result({ main: modelUsage, child: unknown }),
    "turn",
  );
  if (observation?.kind !== "cumulative")
    throw new Error("missing observation");
  expect(observation.total).not.toHaveProperty("reasoningOutput");
  expect(observation.models?.[0]).toMatchObject({
    model: "main",
    providerCostUsd: 0.1,
    tokens: { reasoningOutput: 20 },
  });
});

test("tool identity keeps its original turn across responses and nested tool ownership", () => {
  const usage = new ResponseUsage();
  usage.registerTool("parent", "original");
  const start = events.find((e) => e.type === "message_start");
  if (!start) throw new Error("missing SDK fixture");
  for (const turn of ["later", "even-later"]) {
    usage.observe(frame(start, "parent"), turn);
    expect(
      usage.observe(frame({ type: "message_stop" }, "parent"), turn),
    ).toMatchObject([{ turnId: "original" }]);
  }
  usage.registerTool("nested", usage.turnFor("parent", "current"));
  expect(usage.turnFor("nested", "current")).toBe("original");
});
test("context windows use the response model and its SDK canonical alias", () => {
  const usage = new ResponseUsage();
  usage.recordModels({
    alias: { ...modelUsage, canonicalModel: "claude-opus-5-5" },
  });
  for (const event of events) {
    const observations = usage.observe(frame(event), "turn");
    if (event.type === "message_stop")
      expect(observations).toMatchObject([{ modelContextWindow: 200000 }]);
  }
});

test("query-close accounting is final and late SDK frames cannot repeat it", () => {
  const usage = new ResponseUsage();
  const start = events.find((e) => e.type === "message_start");
  if (!start) throw new Error("missing SDK fixture");
  usage.observe(frame(start, "parent"), "turn");
  expect(usage.finish("queryClosed")).toMatchObject([
    { kind: "gap", responseId: "response-1" },
  ]);
  expect(usage.finish("queryClosed")).toEqual([]);
  for (const event of events)
    expect(usage.observe(frame(event, "parent"), "later")).toEqual([]);
});

test("pricing aliases never overwrite exact model context windows", () => {
  const usage = new ResponseUsage();
  usage.recordModels({
    "claude-opus-5-5": modelUsage,
    "claude-opus-5-5[1m]": {
      ...modelUsage,
      contextWindow: 1000000,
      canonicalModel: "claude-opus-5-5",
    },
  });
  for (const event of events) {
    const observations = usage.observe(frame(event), "turn");
    if (event.type === "message_stop")
      expect(observations).toMatchObject([{ modelContextWindow: 200000 }]);
  }
});
test("a pricing alias with conflicting candidate windows remains unknown", () => {
  const usage = new ResponseUsage();
  usage.recordModels({
    "variant-200k": { ...modelUsage, canonicalModel: "claude-opus-5-5" },
    "variant-1m": {
      ...modelUsage,
      contextWindow: 1000000,
      canonicalModel: "claude-opus-5-5",
    },
  });
  for (const event of events) {
    const observations = usage.observe(frame(event), "turn");
    if (event.type === "message_stop")
      expect(observations[0]).not.toHaveProperty("modelContextWindow");
  }
});

test("one-hour writes are clamped against final stream write totals", () => {
  const start = events.find((e) => e.type === "message_start");
  const delta = events.find((e) => e.type === "message_delta");
  if (!start || !delta) throw new Error("missing SDK fixtures");
  const initial = {
    ...start.message.usage,
    cache_creation: {
      ephemeral_1h_input_tokens: 99,
      ephemeral_5m_input_tokens: 0,
    },
  };
  expect(responseTokens(initial)).toMatchObject({ cacheWriteInput1h: 40 });
  const usage = new ResponseUsage();
  usage.observe(
    frame({ ...start, message: { ...start.message, usage: initial } }),
    "turn",
  );
  usage.observe(
    frame({
      ...delta,
      usage: { ...delta.usage, cache_creation_input_tokens: 6 },
    }),
    "turn",
  );
  expect(
    usage.observe(frame({ type: "message_stop" }), "turn")[0],
  ).toMatchObject({ tokens: { cacheWriteInput: 6, cacheWriteInput1h: 6 } });
  expect(responseTokens({ ...initial, cache_creation: null })).toMatchObject({
    cacheWriteInput1h: 0,
  });
});

for (const stop of ["end_turn", "tool_use"]) {
  test(`provider ${stop} proves finality with final usage`, () => {
    const usage = new ResponseUsage();
    for (const event of events) {
      const next =
        event.type === "message_delta"
          ? {
              ...event,
              delta: {
                ...event.delta,
                stop_reason: /** @type {"end_turn" | "tool_use"} */ (stop),
              },
            }
          : event;
      const observations = usage.observe(frame(next), "turn");
      if (event.type === "message_stop")
        expect(observations).toMatchObject([
          { kind: "response", tokens: { output: 720 } },
        ]);
    }
  });
}
test("CLI synthetic null completion is a gap and a later proper stream can finalize the same id", () => {
  const usage = new ResponseUsage();
  const start = events.find((e) => e.type === "message_start");
  if (!start) throw new Error("missing SDK fixture");
  usage.observe(frame(start), "turn");
  usage.observe(frame(interruptedDelta), "turn");
  expect(usage.observe(frame({ type: "message_stop" }), "turn")).toMatchObject([
    { kind: "gap", responseId: "response-1", reason: "unconfirmedStop" },
  ]);
  expect(usage.finish("streamInterrupted")).toEqual([]);
  for (const event of events) {
    const observations = usage.observe(frame(event), "turn");
    if (event.type === "message_stop")
      expect(observations).toMatchObject([
        { kind: "response", responseId: "response-1", tokens: { output: 720 } },
      ]);
  }
});
test("message_start stop reason alone is not provider delta evidence", () => {
  const usage = new ResponseUsage();
  usage.observe(
    frame({ type: "message_start", message: assistant.message }),
    "turn",
  );
  expect(usage.observe(frame({ type: "message_stop" }), "turn")).toMatchObject([
    { kind: "gap", reason: "unconfirmedStop" },
  ]);
});
test("assistant-only fallback deduplicates without accounting assistant usage; later stream can finalize", () => {
  const usage = new ResponseUsage();
  expect(usage.observeAssistant(assistant, "turn")).toEqual([
    {
      kind: "gap",
      responseId: "response-1",
      model: "claude-opus-5-5",
      turnId: "turn",
      reason: "noStream",
    },
  ]);
  expect(usage.observeAssistant(assistant, "turn")).toEqual([]);
  for (const event of events) {
    const observations = usage.observe(frame(event), "turn");
    expect(usage.observeAssistant(assistant, "turn")).toEqual([]);
    if (event.type === "message_stop")
      expect(observations).toMatchObject([{ kind: "response" }]);
  }
});
test("a hook-shaped id cannot prove a provider call was skipped", () => {
  const usage = new ResponseUsage();
  expect(
    usage.observeAssistant(
      { ...assistant, message: { ...assistant.message, id: "msg_hook_local" } },
      "turn",
    ),
  ).toMatchObject([
    { kind: "gap", reason: "noStream", responseId: "msg_hook_local" },
  ]);
});
