import type { Approval } from "./generated/rustOwned";

export type { Approval };

/** What the request asks. Fixed at creation; it decides which answers fit. */
export type ApprovalKind = Approval["request"]["kind"];
/** The state of one approval: pending, answering, answered, unknown or closed. */
export type ApprovalStateKind = Approval["state"]["kind"];
export type ApprovalOf<K extends ApprovalKind> = Approval & {
  request: Extract<Approval["request"], { kind: K }>;
};

/** Kinds that ask the person questions. */
export const QUESTION_KINDS: readonly ApprovalKind[] = [
  "question",
  "agent_question",
];

export const isPending = (approval: Approval) =>
  approval.state.kind === "pending";

/** The person moved the question aside; the agent may still wait for it. */
export const isDeferred = (approval: Approval) =>
  approval.deferral?.active === true;

/** Pending and not moved aside: it needs the person now. */
export const needsAnswer = (approval: Approval) =>
  isPending(approval) && !isDeferred(approval);

export const isQuestion = (approval: Approval) =>
  QUESTION_KINDS.includes(approval.request.kind);

/** Question that waits for a person and lets the agent keep working. */
export const isAgentQuestion = (approval: Approval) =>
  approval.request.kind === "agent_question";

const text = (value: unknown): string | undefined =>
  typeof value === "string" && value ? value : undefined;

/** Provider fields that no logic reads by name, such as the thread id. */
export const approvalParams = (approval: Approval) => approval.detail.params;

/** The provider transcript item that asked, when known. */
export const approvalItemId = (approval: Approval): string | undefined =>
  approval.request.kind === "patch"
    ? text(approval.request.item)
    : text(approvalParams(approval).itemId);

export const approvalThreadId = (approval: Approval): string | undefined =>
  text(approvalParams(approval).threadId);

/** Seconds since the Unix epoch. */
export const approvalCreated = (approval: Approval): number => approval.created;

/** The first question text of a question request, or undefined. */
export const firstQuestionText = (approval: Approval): string | undefined => {
  const { request } = approval;
  return request.kind === "question" || request.kind === "agent_question"
    ? request.questions[0]?.question
    : undefined;
};

/** One line that says what the request asks. */
export const approvalSummary = (approval: Approval): string | undefined => {
  const { request } = approval;
  switch (request.kind) {
    case "question":
    case "agent_question":
      return firstQuestionText(approval);
    case "exec":
      return request.reason ?? request.command;
    case "patch":
      return request.reason ?? request.files.join(", ");
    case "permissions":
      return request.requested.join(", ");
    case "tool_use":
      return request.tool;
    case "monitor_start":
      return request.command;
    case "browser_open":
      return request.url;
    case "elicitation":
      return text(approvalParams(approval).message);
  }
};
