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
  request: components["schemas"]["RequestEntityDto"],
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
  const request = {
    id: "question",
    agent: "lead",
    method: "agent/asyncQuestion",
    status: "pending",
    params: { questions: [{ id: "choice", question: "Continue?" }] },
  } satisfies components["schemas"]["RequestEntityDto"];
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
    const request = {
      id: "approval",
      agent: "lead",
      method: "item/commandExecution/requestApproval",
      status: "pending",
      params: { command: "pwd" },
    } satisfies components["schemas"]["RequestEntityDto"];
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
    data: { decision: "accept" },
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
    const request = {
      id: "approval",
      agent: "lead",
      method: "item/commandExecution/requestApproval",
      status: "pending",
      params: { threadId, command: "pwd" },
    } satisfies components["schemas"]["RequestEntityDto"];
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

for (const method of [
  "agent/asyncQuestion",
  "item/tool/requestUserInput",
] as const) {
  it(`sends only the answer data for ${method}`, () => {
    h.cells = [{ value: { q: "Chosen" } }];
    h.cursor = 0;
    const post = vi.fn(async () => true);
    const form = AnswerForm({
      draftKey: "form",
      sending: false,
      post,
      close: vi.fn(),
      notify,
      request: {
        id: "q",
        method,
        params: { questions: [{ id: "q", question: "Choose" }] },
      },
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
    request: {
      id: "form",
      method: "mcpServer/elicitation/request",
      params: {
        requestedSchema: {
          properties: { count: { type: "integer" }, flag: { type: "boolean" } },
          required: ["count", "flag"],
        },
      },
    },
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
