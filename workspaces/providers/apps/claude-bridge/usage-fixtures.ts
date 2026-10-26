export const modelUsage: import("@anthropic-ai/claude-agent-sdk").ModelUsage = {
  inputTokens: 10,
  outputTokens: 720,
  thinkingTokens: 20,
  cacheReadInputTokens: 30,
  cacheCreationInputTokens: 40,
  webSearchRequests: 0,
  costUSD: 0.1,
  contextWindow: 200000,
  maxOutputTokens: 32000,
};

export const events: import("@anthropic-ai/claude-agent-sdk").SDKPartialAssistantMessage["event"][] =
  [
    {
      type: "message_start",
      message: {
        id: "response-1",
        type: "message",
        role: "assistant",
        model: "claude-opus-5-5",
        content: [],
        container: null,
        context_management: null,
        diagnostics: null,
        stop_details: null,
        stop_reason: null,
        stop_sequence: null,
        usage: {
          input_tokens: 10,
          output_tokens: 1,
          cache_read_input_tokens: 30,
          cache_creation_input_tokens: 40,
          cache_creation: null,
          fallback_credit: null,
          inference_geo: null,
          iterations: null,
          output_tokens_details: null,
          server_tool_use: null,
          service_tier: null,
          speed: null,
        },
      },
    },
    {
      type: "message_delta",
      context_management: null,
      delta: {
        stop_reason: "end_turn",
        stop_sequence: null,
        container: null,
        stop_details: null,
      },
      usage: {
        input_tokens: 12,
        output_tokens: 720,
        cache_read_input_tokens: 32,
        cache_creation_input_tokens: 42,
        fallback_credit: null,
        iterations: null,
        output_tokens_details: { thinking_tokens: 20 },
        server_tool_use: null,
      },
    },
    { type: "message_stop" },
  ];

export const sdkResult: import("@anthropic-ai/claude-agent-sdk").SDKResultMessage =
  {
    type: "result",
    subtype: "success",
    is_error: false,
    duration_ms: 1,
    duration_api_ms: 1,
    num_turns: 1,
    result: "done",
    stop_reason: "end_turn",
    total_cost_usd: 0.1,
    permission_denials: [],
    uuid: "11111111-1111-4111-8111-111111111111",
    session_id: "session",
    modelUsage: { "claude-opus-5-5": modelUsage },
    usage: {
      input_tokens: 10,
      output_tokens: 720,
      cache_read_input_tokens: 30,
      cache_creation_input_tokens: 40,
      cache_creation: {
        ephemeral_1h_input_tokens: 0,
        ephemeral_5m_input_tokens: 40,
      },
      fallback_credit: null,
      inference_geo: "us",
      iterations: [],
      output_tokens_details: { thinking_tokens: 20 },
      server_tool_use: { web_fetch_requests: 0, web_search_requests: 0 },
      service_tier: "standard",
      speed: "standard",
    },
  };

export const zeroResult: import("@anthropic-ai/claude-agent-sdk").SDKResultMessage =
  {
    ...sdkResult,
    subtype: "error_during_execution",
    is_error: true,
    errors: ["crash"],
    total_cost_usd: 0,
    modelUsage: {},
    usage: {
      ...sdkResult.usage,
      input_tokens: 0,
      output_tokens: 0,
      cache_read_input_tokens: 0,
      cache_creation_input_tokens: 0,
      cache_creation: {
        ephemeral_1h_input_tokens: 0,
        ephemeral_5m_input_tokens: 0,
      },
      output_tokens_details: { thinking_tokens: 0 },
    },
  };

// CLI 2.1.310 Ka(e) synthesizes this nullable delta from e.stopReason/e.usage.

export const interruptedDelta: Extract<
  import("@anthropic-ai/claude-agent-sdk").SDKPartialAssistantMessage["event"],
  { type: "message_delta" }
> = {
  type: "message_delta",
  delta: {
    stop_reason: null,
    stop_sequence: null,
    stop_details: null,
    container: null,
  },
  usage: {
    output_tokens: 1,
    input_tokens: null,
    cache_creation_input_tokens: null,
    cache_read_input_tokens: null,
    server_tool_use: null,
    iterations: null,
    output_tokens_details: null,
    fallback_credit: null,
  },
  context_management: null,
};
const startMessage = events.find((e) => e.type === "message_start");
if (!startMessage) throw new Error("missing SDK message_start fixture");

export const assistant: import("@anthropic-ai/claude-agent-sdk").SDKAssistantMessage =
  {
    type: "assistant",
    message: {
      ...startMessage.message,
      usage: { ...startMessage.message.usage, output_tokens: 720 },
      stop_reason: "end_turn",
      content: [{ type: "text", text: "answer", citations: null }],
    },
    parent_tool_use_id: null,
    uuid: "11111111-1111-4111-8111-111111111111",
    session_id: "session",
  };
