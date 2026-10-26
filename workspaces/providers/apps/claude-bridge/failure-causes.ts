import { claudeFailureSchema } from "./claude-failure.generated.ts";
import type { ClaudeFailure } from "./runtime-contract.generated.ts";
import { isRecord } from "./move.ts";

// Maximum raw diagnostic scan in UTF-16 code units. Raw text is never retained.
const RAW_DETAILS_SCAN_MAX_CHARS = 65_536;

type Rule = Partial<{
  enum: readonly string[];
  type: string;
  minimum: number;
  maximum: number;
  maxLength: number;
  pattern: string;
}>;

// The schema owns each allowed value and bound; this check applies those rules.
function accepted(rule: Rule, value: unknown): boolean {
  if (rule.enum) return typeof value === "string" && rule.enum.includes(value);
  if (rule.type === "integer")
    return (
      typeof value === "number" &&
      Number.isSafeInteger(value) &&
      value >= (rule.minimum ?? -Infinity) &&
      value <= (rule.maximum ?? Infinity)
    );
  return (
    typeof value === "string" &&
    value.length <= (rule.maxLength ?? Infinity) &&
    (rule.pattern === undefined || new RegExp(rule.pattern).test(value))
  );
}

const rules: Record<string, Rule | undefined> = claudeFailureSchema.properties;

// Raw text can quote prompts and credentials. Extract only the native token-count form.
export function claudeFailure(message: unknown): ClaudeFailure | undefined {
  if (!isRecord(message)) return undefined;
  const details = message.error_details;
  const tokens =
    typeof details === "string" && details.length <= RAW_DETAILS_SCAN_MAX_CHARS
      ? details.match(/prompt is too long[^0-9]*(\d+)\s*tokens?\s*>\s*(\d+)/i)
      : null;
  const params = isRecord(message.api_error_params)
    ? message.api_error_params
    : {};
  const candidates: Record<string, unknown> = {
    error: message.error,
    apiError: message.api_error,
    apiErrorStatus: message.api_error_status,
    apiErrorCode: message.api_error_code,
    apiErrorOrigin: message.api_error_origin,
    effort: params.effort,
    provider: params.provider,
    remedy: params.remedy,
    media: params.media,
    mediaReason: params.media_reason,
    promptTokens: tokens ? Number(tokens[1]) : undefined,
    maxContextTokens: tokens ? Number(tokens[2]) : undefined,
  };
  const record: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(candidates)) {
    const rule = rules[key];
    if (rule && accepted(rule, value)) record[key] = value;
  }
  // The runtime check above validated every retained field against the schema.
  return Object.keys(record).length ? (record as ClaudeFailure) : undefined;
}

// The final SDK result owns the cause. Do not attach parameters from a recovered error.
export function terminalClaudeFailure(
  previous: ClaudeFailure | undefined,
  error: unknown,
  result: unknown,
): ClaudeFailure | undefined {
  const last = claudeFailure(error) || claudeFailure(result);
  // An explicit null status means the terminal SDK result has no HTTP failure.
  if (!error && isRecord(result) && result.api_error_status === null) {
    if (!last) return undefined;
    const {
      apiErrorStatus: _status,
      apiErrorOrigin: _origin,
      ...rest
    } = previous ?? {};
    previous = rest;
  }
  if (!last) return previous;
  const differentCause =
    (last.apiError && last.apiError !== previous?.apiError) ||
    (last.apiErrorCode && last.apiErrorCode !== previous?.apiErrorCode) ||
    (last.error && last.error !== previous?.error);
  return differentCause ? last : { ...previous, ...last };
}
