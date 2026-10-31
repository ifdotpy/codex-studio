import { expect, it, vi } from "vitest";
import { isValidElement, type ReactElement, type ReactNode } from "react";
import { initialConfirmedLead } from "../../../tests/teams/concurrency-fixture-data";
const fixture = vi.hoisted(() => ({
  cells: [] as unknown[],
  cursor: 0,
  disk: new Map<string, string>(),
  post: vi.fn(),
}));
vi.mock("react", async (load) => ({
  ...(await load<typeof import("react")>()),
  useState: (initial: unknown) => {
    const index = fixture.cursor++;
    if (!(index in fixture.cells))
      fixture.cells[index] =
        typeof initial === "function" ? initial() : initial;
    return [
      fixture.cells[index],
      (next: unknown) => {
        fixture.cells[index] = next;
      },
    ];
  },
  useRef: (initial: unknown) => {
    const index = fixture.cursor++;
    fixture.cells[index] ??= { current: initial };
    return fixture.cells[index];
  },
  useEffect: () => {},
}));
vi.mock("../../servers/ServerServicesContext", () => ({
  useServerStorage: () => ({
    getItem: (key: string) => fixture.disk.get(key) ?? null,
    setItem: (key: string, value: string) => fixture.disk.set(key, value),
    removeItem: (key: string) => fixture.disk.delete(key),
  }),
  useServerApi: () => ({ post: fixture.post }),
}));
import Control, { decodeStoredConcurrency } from "./SubagentConcurrencyControl";

function nodes(node: ReactNode): ReactElement[] {
  if (Array.isArray(node)) return node.flatMap(nodes);
  if (!isValidElement<{ children?: ReactNode }>(node)) return [];
  return [node, ...nodes(node.props.children)];
}

it("retains and blocks a malformed request introduced while a save is pending", async () => {
  fixture.cells = [];
  fixture.cursor = 0;
  fixture.disk.clear();
  const result = Promise.withResolvers<unknown>();
  fixture.post.mockReturnValue(result.promise);
  const props = {
    lead: initialConfirmedLead,
    stateDir: "state",
    workspaceId: "workspace",
    refresh: async () => {},
  };
  const render = () => {
    fixture.cursor = 0;
    const wrapper = Control(props);
    return (wrapper.type as (props: typeof wrapper.props) => ReactElement)(
      wrapper.props,
    );
  };
  const tree = render();
  const number = nodes(tree).find(
    (node) =>
      node.props &&
      typeof node.props === "object" &&
      "label" in node.props &&
      node.props.label === "Subagent parallelism",
  );
  if (
    !number ||
    !number.props ||
    typeof number.props !== "object" ||
    !("onChange" in number.props) ||
    typeof number.props.onChange !== "function"
  )
    throw new Error("Input missing");
  number.props.onChange(4);
  const form = render();
  if (
    !form.props ||
    typeof form.props !== "object" ||
    !("onSubmit" in form.props) ||
    typeof form.props.onSubmit !== "function"
  )
    throw new Error("Form missing");
  form.props.onSubmit({ preventDefault() {} });
  const key = [...fixture.disk.keys()][0];
  if (!key) throw new Error("Pending request missing");
  expect(JSON.parse(fixture.disk.get(key)!)).not.toHaveProperty("confirmed");
  const raw = JSON.stringify({
    pending: {
      id: "other",
      request_id: "other",
      expected_mode_revision: 0,
      agent_mode: "multi",
    },
  });
  fixture.disk.set(key, raw);
  result.resolve({
    ...initialConfirmedLead,
    concurrency: 4,
    agentModeRevision: 1,
  });
  await vi.waitFor(() => expect(fixture.disk.get(key)).toBe(raw));
  await Promise.resolve();
  const next = render();
  const alert = nodes(next).find(
    (node) =>
      node.props &&
      typeof node.props === "object" &&
      "role" in node.props &&
      node.props.role === "alert",
  );
  expect(alert?.props).toMatchObject({
    children: "The saved concurrency request cannot be read.",
  });
  expect(fixture.disk.get(key)).toBe(raw);
});

it("blocks malformed or other-lead pending requests while preserving valid legacy requests", () => {
  const legacy = {
    id: "lead",
    request_id: "original",
    expected_mode_revision: 2,
    agent_mode: "multi",
  };
  expect(
    decodeStoredConcurrency(JSON.stringify({ pending: legacy }), "lead")
      .pending,
  ).toEqual(legacy);
  expect(() =>
    decodeStoredConcurrency(
      JSON.stringify({ pending: { ...legacy, id: "other" } }),
      "lead",
    ),
  ).toThrow("The saved concurrency request cannot be read.");
  expect(() =>
    decodeStoredConcurrency(
      JSON.stringify({ pending: { ...legacy, expected_mode_revision: "bad" } }),
      "lead",
    ),
  ).toThrow("The saved concurrency request cannot be read.");
  expect(() => decodeStoredConcurrency("null", "lead")).toThrow(
    "The saved concurrency request cannot be read.",
  );
});
