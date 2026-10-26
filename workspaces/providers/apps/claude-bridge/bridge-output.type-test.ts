import type { BridgeResult } from "./bridge-protocol.ts";
import type { BridgeNotificationParams } from "./bridge-protocol.ts";
import type { nativeItem } from "./features.ts";
import type { ActiveQuery } from "./bridge.ts";
import type {
  FreshItem,
  FreshTurn,
  Item,
  Session,
  Turn,
} from "./session-state.ts";

// @ts-expect-error Fresh agent messages always contain text and a phase.
const incompleteAgent: FreshItem = {
  id: "agent",
  type: "agentMessage",
};
void incompleteAgent;
// @ts-expect-error Fresh tool calls carry their tool, arguments, server and status.
const incompleteTool = { id: "tool", type: "mcpToolCall" } satisfies FreshItem;
void incompleteTool;
// @ts-expect-error Future historical item kinds cannot enter fresh producers.
const futureFresh = { id: "future", type: "futureItem" } satisfies FreshItem;
void futureFresh;
const foreignToolBlock = {
  // @ts-expect-error Conversion consumes the SDK tool_use block discriminator.
  type: "future_tool",
  id: "tool",
  name: "Read",
  input: {},
} satisfies Parameters<typeof nativeItem>[0];
void foreignToolBlock;
const nullNativeIdentity = {
  id: "agent",
  type: "agentMessage",
  text: "Reply",
  phase: "commentary",
  // @ts-expect-error Fresh optional identity is omitted, never emitted as null.
  nativeId: null,
} satisfies FreshItem;
void nullNativeIdentity;

export function checkFreshItem(item: FreshItem) {
  if (item.type === "agentMessage") {
    item.text satisfies string;
    item.phase satisfies "commentary" | "final_answer";
    // @ts-expect-error Agent text cannot be treated as a tool call.
    void item.arguments;
  }
  if (item.type === "commandExecution") {
    item.cwd satisfies string;
    item.aggregatedOutput satisfies string | undefined;
    item.status satisfies "inProgress" | "completed" | "failed";
  }
}

export function checkActiveItems(active: ActiveQuery) {
  active.turn?.items satisfies FreshItem[] | undefined;
}
export const omittedCommandFields = {
  id: "command",
  type: "commandExecution",
  status: "completed",
  tool: "Bash",
  arguments: {},
  cwd: "/project",
} satisfies FreshItem;
export const explicitNullCommand = {
  ...omittedCommandFields,
  command: null,
} satisfies FreshItem;
export const omittedFilePath = {
  id: "file",
  type: "fileChange",
  status: "inProgress",
  tool: "Write",
  arguments: {},
  changes: [{ kind: { type: "add" }, diff: "-\n+" }],
} satisfies FreshItem;
export const explicitNullFilePath = {
  ...omittedFilePath,
  changes: [{ path: null, kind: { type: "add" }, diff: "-\n+" }],
} satisfies FreshItem;
export const nullCommandOutput = {
  ...explicitNullCommand,
  // @ts-expect-error Command output can be omitted, but never emitted as null.
  aggregatedOutput: null,
} satisfies FreshItem;

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
