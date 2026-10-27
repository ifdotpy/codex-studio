import type { TurnStatus } from "./codex-generated/TurnStatus.ts";
import type { ThreadStatus } from "./codex-generated/ThreadStatus.ts";
import type { TurnItemsView } from "./codex-generated/TurnItemsView.ts";
import type {
  TerminalTurnStatus,
  ThreadIdentity,
} from "./runtime-contract.generated.ts";
import type { CompletedTurn, Thread } from "./session-state.ts";

// The common terminal domain is exactly the terminal subset of the vendor leaf.
export const toVendor = (
  status: TerminalTurnStatus,
): Exclude<TurnStatus, "inProgress"> => status;
export const fromVendor = (
  status: Exclude<TurnStatus, "inProgress">,
): TerminalTurnStatus => status;
export const incomplete: CompletedTurn = {
  id: "turn",
  // @ts-expect-error A running turn cannot cross the terminal notification boundary.
  status: "inProgress",
};
export const unknown: CompletedTurn = {
  id: "turn",
  // @ts-expect-error Unknown historical statuses have no terminal projection.
  status: "uncertain",
};
// @ts-expect-error A completion always identifies its Studio mirror thread.
export const missingIdentity: ThreadIdentity = {};
export const claudeActive = { type: "active" } satisfies Thread["status"];
// @ts-expect-error Claude activity does not supply Codex's required activeFlags.
export const codexActive: ThreadStatus = claudeActive;
export const fullHistory = "full" satisfies TurnItemsView;
