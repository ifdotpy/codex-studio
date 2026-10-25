import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { beforeEach, expect, it, vi } from "vitest";

const { subscriptions, get, post } = vi.hoisted(() => ({
  subscriptions: new Map<string, unknown>(),
  get: vi.fn(),
  post: vi.fn(),
}));
vi.mock("../../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ get, post }),
  useServerStorage: () => ({}),
}));
vi.mock("../../sync/subscriptions", () => ({
  useSubscription: (ref: unknown) =>
    subscriptions.get(JSON.stringify(ref)) ?? {
      data: undefined,
      status: "loading",
      error: null,
    },
}));
import Tasks from "./Tasks";
import {
  createTaskAssignmentSelector,
  createWorkDataSelector,
} from "../../sync/subscriptions/workData";
import type { SyncEntityPayload } from "../../generated/api";

beforeEach(() => {
  subscriptions.clear();
  get.mockReset();
  post.mockReset();
});

it("assembles ordered canonical histories, isolates roots, and keeps unchanged assignment identities", () => {
  const select = createTaskAssignmentSelector();
  const task = (id: string, rootId = "lead"): SyncEntityPayload => ({
    collection: "workDetail",
    id,
    value: {
      id,
      rootId,
      title: id,
      status: "review",
      created: 1,
      updated: 1,
      version: 1,
    },
  });
  const first = task("first"),
    second = task("second"),
    other = task("private", "other");
  const history: SyncEntityPayload = {
    collection: "workResult",
    id: "first:0",
    value: {
      id: "first:0",
      taskId: "first",
      rootId: "lead",
      index: 0,
      value: {
        id: "result",
        agent: "worker",
        text: "Full review evidence",
        checks: "Checked",
        revision: "abc",
        files: [],
        created: 1,
      },
    },
  };
  const initial = select([first, second, history, other], "lead");
  expect(initial.map((task) => task.id)).toEqual(["first", "second"]);
  expect(initial[0]?.results?.[0]?.text).toBe("Full review evidence");
  expect(select([first, second, history, other], "lead")).toBe(initial);
  const next = select(
    [
      {
        ...first,
        value: { ...first.value, status: "accepted" },
      } as SyncEntityPayload,
      second,
      history,
      other,
    ],
    "lead",
  );
  expect(next[1]).toBe(initial[1]);
  expect(next[0]?.results).toBe(initial[0]?.results);
  expect(next[0]?.results?.[0]).toBe(initial[0]?.results?.[0]);
  expect(select([second], "lead").map((task) => task.id)).toEqual(["second"]);
});

it("renders live typed assignment changes and preserves dependency blocking without state reads", () => {
  const task = {
    id: "assignment",
    rootId: "lead",
    title: "Inspect delivery",
    status: "running",
    owner: "worker",
    version: 1,
    created: 1,
    updated: 1,
    dependencies: ["prerequisite"],
  };
  const prerequisite = {
    ...task,
    id: "prerequisite",
    title: "Prepare evidence",
    status: "ready",
    dependencies: [],
  };
  const selectWork = createWorkDataSelector();
  const setWork = (items: unknown[]) =>
    subscriptions.set(JSON.stringify({ kind: "work", agentId: "lead" }), {
      data: selectWork(
        items.map((value) => ({
          collection: "workDetail",
          id: (value as { id: string }).id,
          value,
        })) as SyncEntityPayload[],
        "lead",
      ),
      status: "live",
      error: null,
    });
  subscriptions.set(JSON.stringify({ kind: "agentDetail", agentId: "lead" }), {
    data: {
      id: "lead",
      taskDefaultGate: { command: "check", timeout_ms: 1234 },
    },
    status: "live",
    error: null,
  });
  const render = () =>
    renderToStaticMarkup(
      createElement(
        MantineProvider,
        {},
        createElement(Tasks, { agentId: "worker", rootId: "lead" }),
      ),
    );
  setWork([task, prerequisite]);
  expect(render()).toContain("Inspect delivery");
  expect(render()).toContain("blocked");
  expect(render()).toContain("check");
  setWork([
    { ...task, status: "review", version: 2 },
    { ...prerequisite, status: "accepted" },
  ]);
  expect(render()).toContain("review");
  expect(render()).toContain("accepted");
  expect(get).not.toHaveBeenCalled();
});

it("keeps an unavailable assignment subscription visible and disables writes before its baseline", () => {
  subscriptions.set(JSON.stringify({ kind: "work", agentId: "lead" }), {
    data: undefined,
    status: "error",
    error: new Error("Assignment stream unavailable"),
  });
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      {},
      createElement(Tasks, { agentId: "worker", rootId: "lead" }),
    ),
  );
  expect(markup).toContain("Assignment stream unavailable");
  expect(markup).toMatch(/<button[^>]*disabled[^>]*>[^]*?Create task/);
  expect(get).not.toHaveBeenCalled();
});
