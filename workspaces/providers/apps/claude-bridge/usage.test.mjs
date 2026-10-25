// @ts-check
import { expect, test } from "vitest";
import { ResponseUsage, cumulativeUsage, modelTokens } from "./usage.mjs";
import { events, modelUsage } from "./usage-fixtures.mjs";
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
  const usage = new ResponseUsage("unrelated", 1000000);
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
