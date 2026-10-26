import type { SDKResultMessage } from "@anthropic-ai/claude-agent-sdk";
import type { TerminalTurnStatus } from "./runtime-contract.generated.ts";

type ClaudeResultLifecycle = Pick<SDKResultMessage, "subtype" | "is_error">;
const resultOutcomes = {
  success: "completed",
  error_during_execution: "failed",
  error_max_turns: "failed",
  error_max_budget_usd: "failed",
  error_max_structured_output_retries: "failed",
} satisfies Record<SDKResultMessage["subtype"], TerminalTurnStatus>;

// Validate only the lifecycle fields we consume, without claiming that this
// checks the full SDK result or changing its immutable native payload.
export function isClaudeResultLifecycle(
  value: unknown,
): value is ClaudeResultLifecycle {
  return (
    value !== null &&
    typeof value === "object" &&
    "is_error" in value &&
    typeof value.is_error === "boolean" &&
    "subtype" in value &&
    typeof value.subtype === "string" &&
    Object.hasOwn(resultOutcomes, value.subtype)
  );
}

// The SDK success subtype can still report an error. Stop retains precedence
// over both that result and a thrown error, as in the existing finalizer.
export function claudeTurnStatus(
  result: ClaudeResultLifecycle | null,
  interrupted: boolean | undefined,
  error: unknown,
): TerminalTurnStatus {
  if (interrupted) return "interrupted";
  if (error || !result || result.is_error) return "failed";
  return resultOutcomes[result.subtype];
}
