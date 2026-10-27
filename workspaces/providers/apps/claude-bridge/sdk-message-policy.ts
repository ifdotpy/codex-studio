import type { SDKMessage } from "@anthropic-ai/claude-agent-sdk";

// These are handling decisions, not a second owner of SDK discriminants.
// Every SDK addition requires an explicit decision before the bridge compiles.
export const handledSdkMessages = {
  assistant: true,
  user: true,
  result: true,
  system: true,
  stream_event: true,
  tool_progress: true,
  auth_status: false,
  tool_use_summary: false,
  rate_limit_event: true,
  prompt_suggestion: false,
  conversation_reset: false,
} satisfies Record<SDKMessage["type"], boolean>;

export const handledSdkSystemMessages = {
  init: true,
  compact_boundary: true,
  status: true,
  api_retry: true,
  control_request_progress: false,
  model_refusal_fallback: true,
  model_refusal_no_fallback: true,
  local_command_output: true,
  hook_started: false,
  hook_progress: false,
  hook_response: false,
  plugin_install: false,
  task_notification: true,
  task_started: true,
  task_updated: true,
  task_progress: true,
  background_tasks_changed: true,
  thinking_tokens: false,
  session_state_changed: false,
  worker_shutting_down: false,
  commands_changed: false,
  notification: true,
  files_persisted: false,
  memory_recall: false,
  elicitation_complete: false,
  permission_denied: false,
  mirror_error: true,
  informational: true,
} satisfies Record<Extract<SDKMessage, { type: "system" }>["subtype"], boolean>;
