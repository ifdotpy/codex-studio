import { errorDetails } from "./errorPresentation.ts";

// Codex 0.153.4 CodexErrorInfo, including unknown future variants.
export const nativeErrorHints: Record<string, string> = {
  contextWindowExceeded: "Compact the context or start a new chat.",
  sessionBudgetExceeded: "Check the session budget before continuing.",
  usageLimitExceeded: "Check this account's limits and reset time.",
  rateLimitExceeded:
    "Check this account's limits and available recovery options.",
  serverOverloaded: "Try later or select another model.",
  cyberPolicy: "Eligible security professionals can apply for Trusted Access.",
  misalignmentPolicyViolation:
    "Codex could not confirm that the agent followed your instructions safely. Start a new chat or open another chat.",
  httpConnectionFailed: "Check the connection before continuing.",
  responseStreamConnectionFailed: "Check the connection before continuing.",
  internalServerError: "Review the last result before continuing.",
  unauthorized: "Sign in again to this chat's account.",
  badRequest: "Check the request and the selected model settings.",
  threadRollbackFailed:
    "Check the conversation before trying another rollback.",
  sandboxError: "Check the command permissions and the project folder.",
  responseStreamDisconnected: "Review the last result before continuing.",
  responseTooManyFailedAttempts:
    "Codex stopped retrying. Review the last result before continuing.",
  activeTurnNotSteerable: "Queue the message until the current turn ends.",
  other: "Review the error details before continuing.",
};

// Bound both JSON input and wrapper depth. Keep the original payload for Details.
const MAX_ERROR_JSON_LENGTH = 65_536;
const MAX_ERROR_DEPTH = 8;

function parseError(value: unknown): any {
  if (typeof value !== "string" || value.length > MAX_ERROR_JSON_LENGTH)
    return value;
  try {
    return JSON.parse(value);
  } catch {
    return value;
  }
}

export function nativeErrorKind(value: unknown): string {
  const data = parseError(value);
  const info = data?.codexErrorInfo ?? data?.data?.codexErrorInfo;
  return (
    (typeof info === "string"
      ? info
      : info && typeof info === "object"
        ? Object.keys(info)[0]
        : "") || ""
  );
}

function biologicalPrefix(text: string) {
  return [
    "Invalid prompt: we've limited access to this content for safety reasons.",
    "This content was flagged for possible biological risk.",
  ].some((prefix) => text.startsWith(prefix));
}

type ReadableError = { message: string; biological: boolean };

// Scan each character once. Only complete outer objects are parsed, so malformed
// or deeply nested input cannot cause repeated parsing of overlapping substrings.
function embeddedObjects(text: string): unknown[] {
  if (text.length > MAX_ERROR_JSON_LENGTH) return [];
  const objects: unknown[] = [];
  let start = -1;
  let depth = 0;
  let quoted = false;
  let escaped = false;
  for (let i = 0; i < text.length; i++) {
    const char = text[i];
    if (start < 0) {
      if (char === "{") {
        start = i;
        depth = 1;
      }
      continue;
    }
    if (quoted) {
      if (escaped) escaped = false;
      else if (char === "\\") escaped = true;
      else if (char === '"') quoted = false;
    } else if (char === '"') quoted = true;
    else if (char === "{") depth++;
    else if (char === "}" && --depth === 0) {
      objects.push(parseError(text.slice(start, i + 1)));
      start = -1;
    }
  }
  return objects;
}

function readableError(value: unknown, depth = 0): ReadableError | undefined {
  if (depth >= MAX_ERROR_DEPTH) return undefined;
  if (typeof value === "string") {
    if (!value.trim()) return undefined;
    const parsed = parseError(value);
    if (parsed !== value) {
      const nested = readableError(parsed, depth + 1);
      if (nested) return nested;
    } else {
      for (const object of embeddedObjects(value)) {
        if (typeof object !== "object" || !object) continue;
        const nested = readableError(object, depth + 1);
        if (nested) return nested;
      }
    }
    return { message: value, biological: biologicalPrefix(value) };
  }
  if (!value || typeof value !== "object" || Array.isArray(value))
    return undefined;
  const data = value as Record<string, unknown>;
  const nested = readableError(data.error, depth + 1);
  const own = nested || readableError(data.message, depth + 1);
  if (own)
    return {
      message: own.message,
      biological: own.biological || data.code === "bio_policy",
    };
  if (data.code === "bio_policy")
    return { message: "Codex reported an error.", biological: true };
  return undefined;
}

export function nativeThreadError(agent?: {
  threadId?: string;
  nativeThreadBlock?: { threadId?: string; error?: unknown };
  error?: unknown;
}): unknown {
  if (agent?.threadId && agent.nativeThreadBlock?.threadId === agent.threadId)
    return agent.nativeThreadBlock.error;
  return nativeErrorKind(agent?.error) === "misalignmentPolicyViolation"
    ? agent?.error
    : undefined;
}

export function nativeErrorView(value: unknown, planType?: string) {
  const data = parseError(value);
  const structured = data && typeof data === "object";
  const kind = nativeErrorKind(data);
  const readable = readableError(data);
  const rawMessage =
    readable?.message ??
    (typeof value === "string" && structured && !kind ? value : undefined);
  const message =
    typeof rawMessage === "string" && rawMessage.trim()
      ? rawMessage
      : kind === "serverOverloaded"
        ? "Codex is currently experiencing high load."
        : structured
          ? "Codex reported an error."
          : "";
  const policy = kind === "cyberPolicy" || Boolean(readable?.biological);
  const links: { label: string; href: string }[] = [];
  if (policy) {
    const individual = ["free", "go", "plus", "pro", "pro_lite"].includes(
      planType || "",
    );
    links.push(
      {
        label: "Trusted Access",
        href:
          kind === "cyberPolicy"
            ? individual
              ? "https://chatgpt.com/cyber/"
              : "https://openai.com/form/enterprise-trusted-access-for-cyber/"
            : "https://chatgpt.com/r/b749fb02595e04c3007a54375f3f4374",
      },
      {
        label: "Learn more",
        href: "https://help.openai.com/en/articles/20001326",
      },
    );
  }
  return {
    kind,
    message,
    title:
      kind === "misalignmentPolicyViolation"
        ? "Chat stopped as a precaution"
        : kind === "usageLimitExceeded"
          ? "Usage limit reached"
          : kind === "rateLimitExceeded"
            ? "Rate limit reached"
            : policy
              ? "This content can't be shown"
              : "",
    hint:
      policy && kind !== "cyberPolicy"
        ? "Codex restricts biological research requests that could pose safety risks. Eligible researchers can apply for Trusted Access."
        : kind
          ? nativeErrorHints[kind] || nativeErrorHints.other
          : "",
    severity: ["serverOverloaded", "activeTurnNotSteerable"].includes(kind)
      ? "warning"
      : policy
        ? "info"
        : "error",
    details:
      typeof value === "string" && value !== message
        ? value
        : structured
          ? errorDetails(data)
          : "",
    additionalDetails:
      structured && typeof data.additionalDetails === "string"
        ? data.additionalDetails
        : "",
    links,
  };
}
