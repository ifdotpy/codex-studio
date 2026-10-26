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
import { RequestCard } from "./Requests";

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
function render(request: components["schemas"]["RequestEntityDto"]) {
  h.cursor = 0;
  return RequestCard({ request, agents: [], scope: "server", refresh, notify });
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
