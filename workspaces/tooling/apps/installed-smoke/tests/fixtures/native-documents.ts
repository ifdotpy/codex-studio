export const usageDocument = {
  input_tokens: 12,
  output_tokens: 3,
  cache_creation_input_tokens: 5,
  cache_creation: { ephemeral_1h_input_tokens: 4 },
} as const;

export const journalDocument = {
  type: "assistant",
  message: { id: "message", model: "fixture", usage: usageDocument },
  payload: { response_id: "response", usage: usageDocument },
} as const;
