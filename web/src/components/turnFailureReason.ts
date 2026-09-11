import type { Message } from "../types";
import { nativeErrorView } from "../nativeErrors";

export const missingFailureReason =
  "The error reason is not available in this history.";

export function failureMessage(value: unknown): string {
  const error = nativeErrorView(value);
  const message = error.title || error.message;
  return !message.trim() ||
    /^(?:failed|error|turn failed|Codex ended this turn with an error\.?|Codex reported an error\.?)$/i.test(
      message.trim(),
    )
    ? missingFailureReason
    : message;
}

// Prefer the model error. A tool's exit code does not explain a model failure.
export function turnFailureReason(items: Message[]): string {
  const native = items.find(
    (item) => item.nativeError && !["tool", "output"].includes(item.role),
  );
  if (native) return failureMessage(native.nativeError);
  const notice = items.find((item) => item.nativeNotice === "error");
  if (notice) return failureMessage(notice.text);
  return missingFailureReason;
}
