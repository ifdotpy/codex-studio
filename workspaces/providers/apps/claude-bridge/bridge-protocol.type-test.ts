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

import type { BridgeHarness } from "./testing/bridge-harness.ts";
export async function checkHarness(bridge: BridgeHarness) {
  const result = await bridge.call("turn/start", start.params);
  result.turn.id satisfies string;
  const completed = await bridge.note(
    "turn/completed",
    (note) => note.params.turn.id === result.turn.id,
  );
  completed.params.turn.id satisfies string;
  const usage = await bridge.note(
    (note) => note.method === "thread/tokenUsage/updated",
  );
  if (usage.params.studioUsage.kind === "gap")
    usage.params.studioUsage.reason satisfies string;
  // @ts-expect-error The harness preserves required steer params.
  await bridge.call("turn/steer", { threadId: "thread", input: [] });
}

export const question = {
  id: "claude:question",
  method: "item/tool/requestUserInput",
  params: {
    threadId: "thread",
    turnId: "turn",
    itemId: "item",
    questions: [
      {
        id: "0",
        header: "Choice",
        question: "Proceed?",
        options: [{ label: "Yes", description: "Continue" }],
        isOther: true,
        isSecret: false,
      },
    ],
  },
} satisfies BridgeServerRequest;

import type {
  BridgeMoveProof,
  BridgeMoveSession,
  BridgeInterruptReceipt,
} from "./bridge-protocol.ts";
import type { Session } from "./session-state.ts";
export function exportedSession(
  session: Session,
  proof: BridgeMoveProof,
): BridgeMoveSession {
  const { session: _snapshot, ...storedProof } = proof;
  return { ...session, moveProof: storedProof, dynamicToolsJson: "[]" };
}
export const legacyInputReceipt = {
  cancelled: [undefined, null],
  still_queued: [],
} satisfies BridgeInterruptReceipt;
export const forwardedPlan = {
  method: "turn/plan/updated",
  params: {
    threadId: "thread",
    turnId: "turn",
    plan: [{ step: 42, status: { future: true } }, {}],
  },
} satisfies BridgeNotification;
