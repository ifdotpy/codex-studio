// Codex 0.153.4 CodexErrorInfo, including unknown future variants.
export const nativeErrorHints: Record<string, string> = {
  contextWindowExceeded: "Compact the context or start a new chat.",
  sessionBudgetExceeded: "Check the session budget before continuing.",
  usageLimitExceeded:
    "Check this account's limits or select another permitted account.",
  rateLimitExceeded: "Wait for this account's limit to reset.",
  serverOverloaded: "Try later or select another model.",
  cyberPolicy: "Review the access requirements for this task.",
  misalignmentPolicyViolation: "Review the request and the policy details.",
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

export function nativeErrorView(value: unknown): {
  message: string;
  hint: string;
  details: string;
} {
  let error = value;
  if (typeof error === "string") {
    try {
      error = JSON.parse(error);
    } catch {
      /* Plain errors remain plain. */
    }
  }
  if (!error || typeof error !== "object")
    return {
      message: typeof error === "string" ? error : "",
      hint: "",
      details: "",
    };
  const data = error as Record<string, any>;
  const info = data.codexErrorInfo ?? data.data?.codexErrorInfo;
  const kind =
    typeof info === "string"
      ? info
      : info && typeof info === "object"
        ? Object.keys(info)[0]
        : "";
  const message =
    typeof data.message === "string"
      ? data.message
      : "Codex reported an error.";
  return {
    message,
    hint: kind ? nativeErrorHints[kind] || nativeErrorHints.other : "",
    details: JSON.stringify(data, null, 2),
  };
}
