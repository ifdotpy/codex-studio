// Codex 0.153.4 CodexErrorInfo, including unknown future variants.
export const nativeErrorHints: Record<string, string> = {
  contextWindowExceeded: "Compact the context or start a new chat.",
  sessionBudgetExceeded: "Check the session budget before continuing.",
  usageLimitExceeded:
    "Check this account's limits or select another permitted account.",
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

function parseError(value: unknown): any {
  let error = value;
  if (typeof error === "string") {
    try {
      error = JSON.parse(error);
    } catch {
      /* Plain errors remain plain. */
    }
  }
  return error;
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

// Match the native TUI's exact prefixes and nested error code, not arbitrary text.
function biologicalPolicy(message: string) {
  const matches = (text: unknown) =>
    typeof text === "string" &&
    [
      "Invalid prompt: we've limited access to this content for safety reasons.",
      "This content was flagged for possible biological risk.",
    ].some((prefix) => text.startsWith(prefix));
  if (matches(message)) return true;
  const nested = parseError(message)?.error;
  return nested?.code === "bio_policy" || matches(nested?.message);
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
  const rawMessage = structured ? data.message : data;
  const message =
    typeof rawMessage === "string" && rawMessage.trim()
      ? rawMessage
      : kind === "serverOverloaded"
        ? "Codex is currently experiencing high load."
        : structured
          ? "Codex reported an error."
          : "";
  const policy = kind === "cyberPolicy" || biologicalPolicy(message);
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
    details: structured ? JSON.stringify(data, null, 2) : "",
    additionalDetails:
      structured && typeof data.additionalDetails === "string"
        ? data.additionalDetails
        : "",
    links,
  };
}
