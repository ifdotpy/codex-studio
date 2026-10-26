import type { ComponentHookCell } from "./componentTestFixtures";
import type { EffectCallback } from "react";
import { required } from "./componentTestFixtures";
import { afterEach, expect, it, vi } from "vitest";
import type { Agent, Snapshot } from "../types";

const h = vi.hoisted(() => ({
  cursor: 0,
  cells: [] as ComponentHookCell[],
  effects: [] as Array<() => unknown>,
  get: vi.fn(async () => ({
    items: [],
    retainedWorktrees: [],
    status: "none",
  })),
  projections: new Set<(snapshot: Snapshot) => void>(),
}));
vi.mock("react", async (original) => {
  const react = await original<typeof import("react")>();
  return {
    ...react,
    useState: (initial: unknown) => {
      const index = h.cursor++;
      h.cells[index] ??= { value: initial };
      return [
        required(h.cells[index]).value,
        (value: unknown) => {
          required(h.cells[index]).value =
            typeof value === "function"
              ? value(required(h.cells[index]).value)
              : value;
        },
      ];
    },
    useMemo: (compute: () => unknown) => compute(),
    useEffect: (effect: EffectCallback, deps: unknown[]) => {
      const index = h.cursor++;
      const previous = h.cells[index];
      if (!previous || deps.some((value, i) => value !== previous.deps?.[i])) {
        previous?.cleanup?.();
        h.cells[index] = { deps };
        h.effects.push(() => {
          required(h.cells[index]).cleanup = effect() || undefined;
        });
      }
    },
  };
});
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ get: h.get }),
  useServerResourceReads: () => (_resource: unknown, read: () => unknown) => {
    void read();
    return () => {};
  },
  useServerFileDialogs: () => ({}),
  useServerSync: () => ({
    subscribeStateProjection: (accept: (snapshot: Snapshot) => void) => {
      h.projections.add(accept);
      return () => h.projections.delete(accept);
    },
  }),
}));
import { useRetainedWorktrees } from "./agents/RetainedWorktrees";
import { useQuestionCards } from "./questions/useQuestionCards";
import ClaudeRepairNotice from "./ClaudeRepairNotice";

function render<T>(fn: () => T) {
  h.cursor = 0;
  const value = fn();
  for (const effect of h.effects.splice(0)) effect();
  return value;
}
afterEach(() => {
  for (const cell of h.cells) cell.cleanup?.();
  h.cells = [];
  h.projections.clear();
  h.effects = [];
  h.get.mockClear();
});

it("retained folders follow source entity changes without cost-triggered HTTP reads", () => {
  const lead = { id: "lead" };
  const read = () => useRetainedWorktrees(lead, []);
  render(read);
  expect(h.get).not.toHaveBeenCalled();
  expect(h.projections.size).toBe(1);
  for (const accept of h.projections)
    accept({
      runtime: {
        retainedWorktrees: [
          {
            id: "worker",
            rootId: "lead",
            cleanup: { phase: "unknown", path: "/kept" },
          },
          { id: "other", rootId: "other-team", cleanup: { phase: "unknown" } },
        ],
      },
    } as unknown as Snapshot);
  expect(render(read).map((row) => row.id)).toEqual(["worker"]);
});

it("question history follows projected answers without a request-triggered HTTP read", () => {
  const item = {
    id: "lead:question-item",
    role: "assistant",
    text: "Continue?",
  };
  const read = () => useQuestionCards("lead", "server", [item], []);
  render(read);
  expect(h.get).not.toHaveBeenCalled();
  expect(h.projections.size).toBe(1);
  for (const accept of h.projections)
    accept({
      runtime: {
        agents: [{ id: "lead", rootId: "lead" }],
        questionHistory: [
          {
            id: "question",
            agent: "lead",
            method: "agent/asyncQuestion",
            status: "answered",
            itemId: "question-item",
            questions: [
              { id: "choice", question: "Continue?", isSecret: false },
            ],
            answerHistory: [
              {
                id: "choice",
                question: "Continue?",
                isSecret: false,
                answer: ["Yes"],
              },
            ],
          },
        ],
      },
    } as unknown as Snapshot);
  expect(required(render(read)(item)?.questions[0]).answers).toEqual(["Yes"]);
  expect(h.get).not.toHaveBeenCalled();
});

it("agent status and completed turns do not refetch saved repair evidence", async () => {
  const agent: Agent = {
    id: "lead",
    threadId: "thread",
    epoch: 1,
    status: "running",
  };
  render(() => ClaudeRepairNotice({ agent }));
  await Promise.resolve();
  render(() =>
    ClaudeRepairNotice({
      agent: { ...agent, status: "idle", lastCompletedTurn: "turn" },
    }),
  );
  await Promise.resolve();
  expect(h.get).toHaveBeenCalledTimes(1);
});
