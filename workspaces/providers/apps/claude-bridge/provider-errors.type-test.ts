import type { SDKAssistantMessage } from "@anthropic-ai/claude-agent-sdk";
import type {
  CodexErrorInfo,
  KnownCodexErrorKind,
} from "./provider-errors.generated.ts";
import { claudeErrorInfo, legacyClaudeErrorInfo } from "./features.ts";

export function checkAssistantErrorBoundary(
  message: SDKAssistantMessage,
  external: unknown,
) {
  const known: KnownCodexErrorKind | null = claudeErrorInfo(
    message.error,
    "Failure",
  );
  // @ts-expect-error Untyped exceptions must enter through the explicit adapter.
  claudeErrorInfo(external, "Failure");
  // @ts-expect-error Future SDK values require a classification decision at upgrade.
  claudeErrorInfo("future_sdk_error", "Failure");
  return { known, external: legacyClaudeErrorInfo(external, "Failure") };
}

// @ts-expect-error Unknown raw vendor tags cannot enter the known classification.
export const unknownKind: KnownCodexErrorKind = "future_vendor_error";
export const unknownRecord = {
  future_vendor_error: { opaque: [1, null, { retain: true }] },
} satisfies CodexErrorInfo;
