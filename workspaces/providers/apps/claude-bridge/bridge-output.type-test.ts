import type { BridgeResult } from "./bridge-protocol.ts";
import type { BridgeNotificationParams } from "./bridge-protocol.ts";
import type { FreshTurn, Item, Session, Turn } from "./session-state.ts";

// Older disk records remain readable without fresh output guarantees.
const legacyItem = {} satisfies Item;
const legacyTurn = { id: "old", items: [legacyItem] } satisfies Turn;
void legacyTurn;
const missingInitialItemIdentity = {
  id: "turn",
  status: "inProgress",
  // @ts-expect-error Newly constructed turns require identified items.
  items: [{}],
} satisfies FreshTurn;
void missingInitialItemIdentity;

const missingItemIdentity = {
  threadId: "thread",
  // @ts-expect-error A fresh completed item requires its id and type.
  item: {},
} satisfies BridgeNotificationParams<"item/completed">;
void missingItemIdentity;
const missingTerminalStatus = {
  threadId: "thread",
  // @ts-expect-error A completed turn always carries a terminal status.
  turn: { id: "turn" },
} satisfies BridgeNotificationParams<"turn/completed">;
void missingTerminalStatus;
const nonterminalCompletion = {
  threadId: "thread",
  // @ts-expect-error An active turn cannot be emitted as completed.
  turn: { id: "turn", status: "inProgress" },
} satisfies BridgeNotificationParams<"turn/completed">;
void nonterminalCompletion;
const inventedStartOutcome = {
  threadId: "thread",
  // @ts-expect-error Fresh completions only emit the proven not_applied outcome.
  turn: { id: "turn", status: "failed", startOutcome: "invented" },
} satisfies BridgeNotificationParams<"turn/completed">;
void inventedStartOutcome;

const moveOverrides = {
  effort: "high",
  serviceTier: "priority",
} satisfies NonNullable<Session["moveTurnOptions"]>;
void moveOverrides;
const unrelatedSdkOption = {
  // @ts-expect-error Move overrides contain bridge effort/serviceTier only.
  cwd: "/project",
} satisfies NonNullable<Session["moveTurnOptions"]>;
void unrelatedSdkOption;

const invalidThreadPage = {
  data: [],
  // @ts-expect-error nextCursor is a string or null in the thread/list result.
  nextCursor: 17,
} satisfies BridgeResult<"thread/list">;
void invalidThreadPage;
