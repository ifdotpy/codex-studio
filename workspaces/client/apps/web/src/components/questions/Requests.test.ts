import { QuestionDecision } from "../../generated/api";
import { approval, question } from "../../approvalFixtures";
import type { Approval } from "../../approvals";
import { isValidElement, type ReactElement, type ReactNode } from "react";
import { afterEach, expect, it, vi } from "vitest";
import type { components } from "../../generated/api";
import { required, type ComponentHookCell } from "../componentTestFixtures";

const h = vi.hoisted(() => ({
  cursor: 0,
  cells: [] as ComponentHookCell[],
  storage: {},
  post: vi.fn(async () => ({})),
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (initial: unknown) => {
    const index = h.cursor++;
    h.cells[index] ??= {
      value: typeof initial === "function" ? initial() : initial,
    };
    return [
      required(h.cells[index]).value,
      (value: unknown) => {
        const cell = required(h.cells[index]);
        cell.value = typeof value === "function" ? value(cell.value) : value;
      },
    ];
  },
  useRef: (current: unknown) => {
    const index = h.cursor++;
    h.cells[index] ??= { current };
    return h.cells[index];
  },
  useEffect: () => {},
}));
vi.mock("../../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ post: h.post }),
  useServerServices: () => null,
  useServerStorage: () => h.storage,
  useServerLocalDrafts: () => ({ writeLocalDraft: () => null }),
}));
import { AnswerForm, RequestCard } from "./Requests";

type NodeProps = {
  children?: ReactNode;
  "data-request"?: string;
  "data-answer"?: string;
  "aria-expanded"?: boolean;
  className?: string;
  onClick?: () => void;
};
function nodes(node: ReactNode): ReactElement<NodeProps>[] {
  if (Array.isArray(node)) return node.flatMap(nodes);
  if (!isValidElement<NodeProps>(node)) return [];
  return [node, ...nodes(node.props.children)];
}
const refresh = vi.fn(async () => {});
const notify = vi.fn();
function render(
  request: Approval,
  agents: Parameters<typeof RequestCard>[0]["agents"] = [],
) {
  h.cursor = 0;
  return RequestCard({ request, agents, scope: "server", refresh, notify });
}
afterEach(() => {
  h.cells = [];
  h.post.mockClear();
  refresh.mockClear();
  notify.mockClear();
});

it("Answer expands and hides the editor inside the same request card", () => {
  const request = approval({
    id: "question",
    request: {
      kind: "agent_question",
      questions: [question("choice", "Continue?")],
    },
  });
  const closed = render(request);
  const answer = required(
    nodes(closed).find((node) => node.props["data-answer"] === request.id),
  );
  expect(answer.props["aria-expanded"]).toBe(false);
  required(answer.props.onClick)();
  const opened = render(request);
  expect(opened.type).toBe(closed.type);
  expect(opened.key).toBe(closed.key);
  expect(opened.props["data-request"]).toBe(request.id);
  expect(
    nodes(opened).filter((node) => node.props["data-request"]),
  ).toHaveLength(1);
  expect(
    nodes(opened).filter(
      (node) => node.props.className === "request-inline-answer",
    ),
  ).toHaveLength(1);
  required(
    required(
      nodes(opened).find((node) => node.props["data-answer"] === request.id),
    ).props.onClick,
  )();
  expect(
    nodes(render(request)).filter(
      (node) => node.props.className === "request-inline-answer",
    ),
  ).toHaveLength(0);
  expect(h.post).not.toHaveBeenCalled();
});

for (const decision of ["accept", "decline"] as const) {
  it(`approval ${decision} retains its card and sends the exact request once`, async () => {
    const request = approval({
      id: "approval",
      request: { kind: "exec", command: "pwd", cwd: "/", reason: null },
    });
    const before = render(request);
    const action = required(
      nodes(before).find(
        (node) =>
          node.props.children ===
          (decision === "accept" ? "Approve" : "Decline"),
      ),
    );
    required(action.props.onClick)();
    required(action.props.onClick)();
    const sending = render(request);
    expect(sending.type).toBe(before.type);
    expect(sending.props["data-request"]).toBe(request.id);
    expect(
      nodes(sending).filter((node) => node.props["data-request"]),
    ).toHaveLength(1);
    expect(h.post).toHaveBeenCalledTimes(1);
    expect(h.post).toHaveBeenCalledWith("/api/answer", {
      id: request.id,
      decision,
    });
    await vi.waitFor(() => expect(refresh).toHaveBeenCalledTimes(1));
    expect(notify).not.toHaveBeenCalled();
  });
}

// These deliberately invalid fresh answers must fail against the same generated
// contract used by AnswerForm; opaque legacy HTTP answers keep their own boundary.
function questionAnswerConstructionWitness() {
  type Answer = components["schemas"]["QuestionCardAnswer"];
  // @ts-expect-error a question answer requires its selected-answer map
  const missingAnswers: Answer = { method: "agent/asyncQuestion", data: {} };
  // @ts-expect-error elicitation requires content alongside its decision
  const missingContent: Answer = {
    method: "mcpServer/elicitation/request",
    data: { decision: QuestionDecision.accept },
  };
  const wrongMethod: Answer = {
    method: "item/commandExecution/requestApproval",
    // @ts-expect-error an approval cannot use the question-answer payload
    data: { answers: {} },
  };
  void [missingAnswers, missingContent, wrongMethod];
}
void questionAnswerConstructionWitness;

for (const threadId of [null, "thread-1"]) {
  it(`keeps the native thread refusal scoped to a real matching thread (${threadId})`, () => {
    const request = approval({
      id: "approval",
      request: { kind: "exec", command: "pwd", cwd: "/", reason: null },
      params: { threadId },
    });
    const card = render(request, [
      {
        id: "lead",
        threadId,
        error: { codexErrorInfo: "misalignmentPolicyViolation" },
      },
    ]);
    const approve = nodes(card).find(
      (node) => node.props.children === "Approve",
    );
    expect(Boolean(approve)).toBe(threadId === null);
    if (approve) {
      required(approve.props.onClick)();
      expect(h.post).toHaveBeenCalledWith("/api/answer", {
        id: "approval",
        decision: "accept",
      });
    } else expect(h.post).not.toHaveBeenCalled();
  });
}

for (const kind of ["agent_question", "question"] as const) {
  it(`sends only the answer data for ${kind}`, () => {
    h.cells = [{ value: { q: "Chosen" } }];
    h.cursor = 0;
    const post = vi.fn(async () => true);
    const form = AnswerForm({
      draftKey: "form",
      sending: false,
      post,
      close: vi.fn(),
      notify,
      request: approval({
        id: "q",
        request: { kind, questions: [question("q", "Choose")] },
      }),
    });
    if (
      !isValidElement<{
        onSubmit: (event: { preventDefault(): void }) => void;
      }>(form)
    )
      throw new Error("Expected answer form");
    form.props.onSubmit({ preventDefault() {} });
    expect(post).toHaveBeenCalledWith({
      answers: { q: { answers: ["Chosen"] } },
    });
  });
}

it("sends elicitation content through its generated answer variant", () => {
  h.cells = [{ value: { count: "7", flag: "true" } }];
  h.cursor = 0;
  const post = vi.fn(async () => true);
  const form = AnswerForm({
    draftKey: "form",
    sending: false,
    post,
    close: vi.fn(),
    notify,
    request: approval({
      id: "form",
      request: {
        kind: "elicitation",
        schema: {
          properties: { count: { type: "integer" }, flag: { type: "boolean" } },
          required: ["count", "flag"],
        },
      },
      params: { mode: "form" },
    }),
  });
  if (
    !isValidElement<{ onSubmit: (event: { preventDefault(): void }) => void }>(
      form,
    )
  )
    throw new Error("Expected answer form");
  form.props.onSubmit({ preventDefault() {} });
  expect(post).toHaveBeenCalledWith({
    decision: "accept",
    content: { count: 7, flag: true },
  });
});

const question1: Approval["request"] = {
  kind: "agent_question",
  questions: [question("q", "Continue?")],
};
const sent: Extract<Approval["state"], { kind: "answered" }>["answer"] = {
  kind: "answers",
  answers: [{ id: "q", answer: ["Yes"] }],
};
const stateCases: [Approval["state"], boolean][] = [
  [{ kind: "pending" }, true],
  [{ kind: "answering", answer: sent, command: "command" }, false],
  [{ kind: "answered", answer: sent, by: { kind: "owner" }, at: 2 }, false],
  [{ kind: "unknown", answer: sent, since: 2 }, false],
  [{ kind: "closed", reason: "expired", at: 2 }, false],
];
for (const [state, controls] of stateCases) {
  it(`shows answer controls only while a question is pending (${state.kind})`, () => {
    const card = render(approval({ request: question1, state }));
    expect(
      nodes(card).some((node) => node.props["data-answer"] === "approval"),
    ).toBe(controls);
  });
}

const approvalCases: Approval["request"][] = [
  { kind: "patch", item: "item", files: ["a.ts"], reason: null },
  { kind: "permissions", requested: ["network"] },
  { kind: "tool_use", tool: "Bash", input: {} },
  { kind: "monitor_start", monitor: "m", command: "tail -f log" },
];
for (const request of approvalCases) {
  it(`offers approve and decline for a ${request.kind} request`, () => {
    const labels = nodes(render(approval({ request }))).map(
      (node) => node.props.children,
    );
    expect(labels).toContain("Approve");
    expect(labels).toContain("Decline");
  });
}
