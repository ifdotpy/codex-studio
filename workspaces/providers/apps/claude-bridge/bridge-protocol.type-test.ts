import type {
  BridgeRequest,
  BridgeNotification,
  BridgeMethodResult,
  BridgeServerRequest,
} from "./bridge-protocol.ts";

export const start = {
  id: 1,
  method: "turn/start",
  params: {
    threadId: "thread",
    input: [{ type: "text", text: "Hello", inputOrigin: "human" }],
  },
} satisfies BridgeRequest;
export const accepted = {
  method: "turn/start",
  result: { turn: { id: "turn", status: "inProgress" } },
} satisfies BridgeMethodResult;
export const usage = {
  method: "thread/tokenUsage/updated",
  params: {
    threadId: "thread",
    studioUsage: { kind: "gap", responseId: "response", reason: "queryClosed" },
  },
} satisfies BridgeNotification;
export const approval = {
  id: "claude:request",
  method: "item/commandExecution/requestApproval",
  params: {
    threadId: "thread",
    turnId: "turn",
    itemId: "item",
    command: "true",
    cwd: "/project",
    reason: "Requested permission",
  },
} satisfies BridgeServerRequest;
export const malformed: BridgeRequest<"turn/steer"> = {
  method: "turn/steer",
  // @ts-expect-error A steer requires the expected turn and original input identity.
  params: { threadId: "thread", input: [] },
};
export const malformedUsage: BridgeNotification<"thread/tokenUsage/updated"> = {
  method: "thread/tokenUsage/updated",
  params: {
    threadId: "thread",
    // @ts-expect-error A gap reason must come from the generated usage contract.
    studioUsage: { kind: "gap", responseId: "response", reason: "invented" },
  },
};
