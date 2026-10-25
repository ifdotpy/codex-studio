import { claudeFailureSchema } from "./claude-failure.generated.mjs";

// Maximum raw diagnostic scan in UTF-16 code units. Raw text is never retained.
const RAW_DETAILS_SCAN_MAX_CHARS = 65_536;

function accepted(rule, value) {
  if (rule.enum) return rule.enum.includes(value);
  if (rule.type === "integer")
    return (
      Number.isSafeInteger(value) &&
      value >= rule.minimum &&
      value <= rule.maximum
    );
  return (
    typeof value === "string" &&
    value.length <= rule.maxLength &&
    new RegExp(rule.pattern).test(value)
  );
}

// Raw text can quote prompts and credentials. Extract only the native token-count form.
export function claudeFailure(message) {
  const tokens =
    typeof message?.error_details === "string" &&
    message.error_details.length <= RAW_DETAILS_SCAN_MAX_CHARS
      ? message.error_details.match(
          /prompt is too long[^0-9]*(\d+)\s*tokens?\s*>\s*(\d+)/i,
        )
      : null;
  const params = message?.api_error_params;
  const candidates = {
    error: message?.error,
    apiError: message?.api_error,
    apiErrorStatus: message?.api_error_status,
    apiErrorCode: message?.api_error_code,
    apiErrorOrigin: message?.api_error_origin,
    effort: params?.effort,
    provider: params?.provider,
    remedy: params?.remedy,
    media: params?.media,
    mediaReason: params?.media_reason,
    promptTokens: tokens ? Number(tokens[1]) : undefined,
    maxContextTokens: tokens ? Number(tokens[2]) : undefined,
  };
  const record = Object.fromEntries(
    Object.entries(candidates).filter(([key, value]) =>
      accepted(claudeFailureSchema.properties[key], value),
    ),
  );
  return Object.keys(record).length ? record : undefined;
}

// The final SDK result owns the cause. Do not attach parameters from a recovered error.
export function terminalClaudeFailure(previous, error, result) {
  const last = claudeFailure(error) || claudeFailure(result);
  // An explicit null status means the terminal SDK result has no HTTP failure.
  if (!error && result?.api_error_status === null) {
    if (!last) return undefined;
    previous = { ...previous };
    delete previous.apiErrorStatus;
    delete previous.apiErrorOrigin;
  }
  if (!last) return previous;
  const differentCause =
    (last.apiError && last.apiError !== previous?.apiError) ||
    (last.apiErrorCode && last.apiErrorCode !== previous?.apiErrorCode) ||
    (last.error && last.error !== previous?.error);
  return differentCause ? last : { ...previous, ...last };
}
