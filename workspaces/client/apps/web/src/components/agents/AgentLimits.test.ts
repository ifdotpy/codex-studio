import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test, vi } from "vitest";
import type { Agent } from "../../types";
vi.mock("../../servers/ServerServicesContext", () => ({
  useServerApi: () => ({
    post: vi.fn(),
    saved: (_key: string, value: unknown) => value,
  }),
  useServerStorage: () => ({ setItem: vi.fn() }),
  useServerResourceReads: () => vi.fn(),
  useServerFileDialogs: () => ({
    canRevealPath: () => true,
    revealPath: vi.fn(),
  }),
}));
import { WorkerLimits, RetainedWorktree } from "./AgentLimits";
import { budgetInput, TeamBudgetView } from "./TeamBudget";
import { workerState } from "./WorkerOverview";
function render(component: ReturnType<typeof createElement>) {
  return renderToStaticMarkup(createElement(MantineProvider, null, component));
}
const worker: Agent = {
  id: "worker",
  worktree: true,
  rootId: "lead",
  status: "completed",
  completedWorkerTurns: 2,
  terminalReason: "worker_max_turns",
  capabilityProfile: {
    allowedTools: ["Read*"],
    disallowedTools: ["Agent"],
    mcpServers: [],
    skills: ["research"],
    maxTurns: 2,
  },
};

test("exhausted workers show stopped state, count, effective limits, and Continue", () => {
  const html = render(createElement(WorkerLimits, { agent: worker }));
  for (const value of [
    "Stopped: turn limit reached",
    "Turns used: 2 / 2",
    "Continue",
    "Effective capabilities",
    "Read*",
    "Agent",
    "No user MCP servers",
    "research",
  ])
    expect(html).toContain(value);
  expect(workerState(worker, new Set())).toBe("stopped");
});
test("unlimited workers show provider defaults without a reset action", () => {
  const html = render(createElement(WorkerLimits, { agent: { id: "other" } }));
  expect(html).toContain("Turns used: 0 / Unlimited");
  expect(html).toContain("Provider default");
  expect(html).not.toContain(">Continue<");
});
test("lead and remote worker panels do not offer a local continuation", () => {
  expect(
    render(createElement(WorkerLimits, { agent: { ...worker, isLead: true } })),
  ).not.toContain("Worker limits");
  const html = render(
    createElement(WorkerLimits, {
      agent: {
        ...worker,
        remoteWorker: { server: "remote", link: "remote-worker" },
      },
    }),
  );
  expect(html).toContain("Stopped: turn limit reached");
  expect(html).not.toContain(">Continue<");
});
test("retained worktrees show reason and path without a delete action", () => {
  const html = render(
    createElement(RetainedWorktree, {
      agent: {
        ...worker,
        workspaceCleanupIntent: {
          phase: "not_sent",
          reason: "Submodule vendor has changes",
          path: "/work/retained",
        },
      },
    }),
  );
  for (const value of [
    "Worktree retained",
    "Submodule vendor has changes",
    "/work/retained",
    "Open folder",
  ])
    expect(html).toContain(value);
  expect(html).not.toContain("Delete");
  expect(
    render(createElement(RetainedWorktree, { agent: worker })),
  ).not.toContain("Worktree retained");
});
test("team totals distinguish verified remaining tokens from incomplete usage", () => {
  const summary = {
    rootId: "lead",
    tokenBudget: 100,
    tokensUsed: 40,
    reached: false,
    incomplete: [],
  };
  const verified = render(createElement(TeamBudgetView, { summary }));
  expect(verified).toContain("Budget: 100 tokens");
  expect(verified).toContain("Remaining: 60 tokens");
  const html = render(
    createElement(TeamBudgetView, {
      summary: {
        ...summary,
        incomplete: [
          { agentId: "deleted", reason: "Native usage history is unavailable" },
        ],
      },
      blocked: "Usage accounting incomplete",
    }),
  );
  expect(html).toContain("Spent: 40 tokens (known usage)");
  expect(html).toContain("Remaining: Unavailable");
  expect(html).toContain("Usage accounting incomplete.");
  expect(html).toContain("Admission blocked:");
  expect(
    render(
      createElement(TeamBudgetView, {
        summary: { ...summary, tokenBudget: null },
      }),
    ),
  ).toContain("Budget: Unlimited");
  expect(
    render(
      createElement(TeamBudgetView, {
        summary: { ...summary, tokensUsed: 120, reached: true },
      }),
    ),
  ).toContain("Remaining: 0 tokens");
});
test("budget input supports an unlimited cap and rejects invalid token counts", () => {
  expect(budgetInput(" ")).toBeNull();
  expect(budgetInput("12345")).toBe(12345);
  for (const value of ["0", "-1", "1.5", "NaN", "9007199254740992"])
    expect(() => budgetInput(value)).toThrow();
});
