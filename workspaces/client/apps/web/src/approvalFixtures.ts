import type { Approval } from "./approvals";

type Fixture = {
  request: Approval["request"];
  id?: string;
  agent?: string;
  turn?: string | null;
  created?: number;
  epoch?: number | null | undefined;
  state?: Approval["state"];
  /** Provider fields kept in `detail.params`, such as `itemId` or `threadId`. */
  params?: Approval["detail"]["params"];
  deferred?: boolean;
};

/** A typed approval row for tests and stories. */
export function approval({
  request,
  id = "approval",
  agent = "lead",
  turn = "turn",
  created = 1,
  epoch = null,
  state = { kind: "pending" },
  params = {},
  deferred = false,
}: Fixture): Approval {
  return {
    id,
    agent,
    turn,
    created,
    epoch,
    request,
    delivery: null,
    detail: {
      params,
      preview: null,
      answer_error: null,
      host_close_reason: null,
    },
    state,
    deferral: deferred
      ? { active: true, at: created, by: { kind: "owner" } }
      : null,
    fire_at: null,
    hidden: null,
  };
}

/** A question with the fields the answer form needs. */
export function question(
  id: string,
  text: string,
  options: string[] = [],
): Extract<Approval["request"], { kind: "question" }>["questions"][number] {
  return {
    id,
    question: text,
    header: null,
    options: options.map((label) => ({
      label,
      description: null,
      preview: null,
    })),
    is_secret: false,
    multi_select: null,
    is_other: null,
  };
}

type SimpleRequest = {
  id?: string;
  agent: string;
  epoch?: number | null | undefined;
  deferred?: boolean;
  /** An answered request instead of a pending one. */
  answered?: boolean;
};

/** A request that asks for a command, pending unless `answered`. */
export function simpleRequest({
  answered = false,
  ...rest
}: SimpleRequest): Approval {
  return approval({
    request: { kind: "exec", command: "pwd", cwd: "/", reason: null },
    ...rest,
    ...(answered
      ? {
          state: {
            kind: "answered",
            answer: { kind: "decision", decision: "accept" },
            by: { kind: "owner" },
            at: 2,
          },
        }
      : {}),
  });
}
