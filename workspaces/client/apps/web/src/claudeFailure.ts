import type { components } from "./generated/api";

type ClaudeFailure = components["schemas"]["ClaudeFailure"];

// Presentation only. codexErrorInfo remains the policy and recovery key.
const causes: Record<string, [string, string]> = {
  tool_history_mismatch: [
    "Tool history does not match",
    "The conversation's tool calls and results do not match. Rewind the conversation or start a new chat.",
  ],
  autocompact_thrashing: [
    "Context fills again after compaction",
    "The context reaches its limit after repeated compaction. Remove large inputs or start a new chat.",
  ],
  media_removed: [
    "Image or document removed",
    "Claude could not use an image or document. Later requests can omit it. Check the attachment before continuing.",
  ],
  provider_credentials: [
    "Provider credentials failed",
    "The cloud provider refused the credentials or access. Check this account's sign-in and model access.",
  ],
  dlp_request_denied: [
    "Organization policy refused the request",
    "The organization's data protection policy refused this request. Review the request before continuing.",
  ],
  safety_monitor_blocked: [
    "Safety policy blocked the response",
    "Claude's safety monitor blocked this response. Review the request before continuing.",
  ],
  max_output_tokens: [
    "Output token limit reached",
    "Claude reached the output token limit. Request a shorter answer.",
  ],
  claude_code_version_too_old: [
    "Claude Code update required",
    "Update Claude Code before continuing.",
  ],
  effort_requires_thinking: [
    "Thinking is required",
    "The selected effort level requires thinking. Check the model settings.",
  ],
  advisor_incompatible: [
    "Advisor model is incompatible",
    "The advisor cannot use the selected model. Check the model settings.",
  ],
  pdf_too_large: [
    "PDF exceeds the limit",
    "The PDF exceeds the page or size limit. Use a smaller document.",
  ],
  pdf_password_protected: [
    "PDF is password protected",
    "Remove the PDF password before attaching the document.",
  ],
  no_response: [
    "No API response",
    "No response arrived before the deadline. Check the connection or gateway.",
  ],
  tls_untrusted_ca: [
    "API certificate is not trusted",
    "Check the certificate authority for the API connection.",
  ],
  gateway_content_type: [
    "Gateway response is incompatible",
    "The gateway changed the response format. Check the gateway settings.",
  ],
  gateway_signin_required: [
    "Gateway sign-in required",
    "Sign in to the configured gateway before continuing.",
  ],
  gateway_session_expired: [
    "Gateway session expired",
    "Refresh the gateway credentials before continuing.",
  ],
  api_key_auth_disabled: [
    "API key sign-in is disabled",
    "The organization disabled API key authentication. Use an allowed sign-in method.",
  ],
  org_disabled_credential: [
    "Credential organization is disabled",
    "The active API key belongs to a disabled organization. Check the account credentials.",
  ],
  invalid_credential_header: [
    "Credential header is invalid",
    "A credential cannot be sent as an HTTP header. Check the account credentials.",
  ],
  model_requires_usage_credits: [
    "Model requires usage credits",
    "Check this account's usage credits or select another model.",
  ],
  long_context_credits_required: [
    "Long context requires usage credits",
    "Enable usage credits or reduce the context.",
  ],
  consent_unanswered: [
    "Usage credit choice is unanswered",
    "Confirm the usage credit choice before continuing.",
  ],
  no_allowed_fallback: [
    "No allowed fallback model",
    "The model policy allows no fallback. Check the allowed models.",
  ],
  model_substitution_disabled: [
    "Model fallback is disabled",
    "Check the model fallback setting before continuing.",
  ],
  field_not_granted: [
    "Request field is not allowed",
    "The organization cannot use a request field. Check the selected settings.",
  ],
  usage_limit_reached: [
    "Claude usage limit reached",
    "Check this account's limit and reset time.",
  ],
};

export function claudeFailureView(value: unknown) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const record = value as ClaudeFailure;
  const classDescriptions: Record<string, [string, string]> = {
    authentication_failed: [
      "Claude sign-in failed",
      "Sign in again to this account.",
    ],
    cloud_credential_error: [
      "Provider credentials failed",
      "Check the cloud provider credentials.",
    ],
    oauth_org_not_allowed: [
      "Organization sign-in is not allowed",
      "Use an account in an allowed organization.",
    ],
    account_on_hold: [
      "Claude account is on hold",
      "Check this account's status.",
    ],
    verification_required: [
      "Account verification is required",
      "Complete this account's verification.",
    ],
    billing_error: [
      "Claude billing failed",
      "Check this account's billing settings.",
    ],
    rate_limit: [
      "Rate limit reached",
      "Check this account's limit and reset time.",
    ],
    overloaded: [
      "Claude is at capacity",
      "Try again later or select another model.",
    ],
    invalid_request: [
      "Claude request is invalid",
      "Check the request and model settings.",
    ],
    model_not_found: [
      "Claude model is unavailable",
      "Check the selected model and account access.",
    ],
    server_error: [
      "Claude server failed",
      "Review the last result before continuing.",
    ],
    max_output_tokens: causes.max_output_tokens,
  };
  const known =
    typeof record.apiError === "string" ? causes[record.apiError] : undefined;
  const [title, description] = known ||
    (record.error ? classDescriptions[record.error] : undefined) || [
      "Claude request failed",
      "Claude could not complete the request. Check the cause details before continuing.",
    ];
  const counts =
    Number.isSafeInteger(record.promptTokens) &&
    Number.isSafeInteger(record.maxContextTokens)
      ? ` Context: ${record.promptTokens} tokens. Limit: ${record.maxContextTokens} tokens.`
      : "";
  return { title, message: description + counts, hint: description + counts };
}
