import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import type { Agent, Snapshot } from "../../types";
import { chatIndicators } from "../chat-status/chatStatusModel";
import TeamPanel from "./TeamPanel";

const now = Math.floor(Date.now() / 1000);
const lead: Agent = {
  id: "lead",
  name: "Studio release",
  role: "lead",
  isLead: true,
  source: "managed",
  status: "running",
  inFlight: true,
  model: "gpt-6-astra",
  created: now - 195,
  activity: { phase: "thinking", at: now - 32 },
};
const workerFixtures: Agent[] = [
  {
    id: "question",
    name: "Permissions review",
    status: "approval",
    role: "reviewer",
    overview: { task: "Confirm the allowed command scope" },
  },
  {
    id: "error",
    name: "Обновление индексов для нескольких проектов и проверка восстановления после перезапуска",
    status: "failed",
    error: "Type check failed in the project adapter.",
    overview: { task: "Repair the project migration" },
  },
  {
    id: "running",
    name: "Transcript worker",
    status: "running",
    inFlight: true,
    activity: { phase: "tool", at: now - 74 },
    commandProgress: [
      {
        monitorId: "typecheck",
        stage: "pnpm run typecheck:fast",
        startedAt: now - 74,
        lastOutputLine: "Checking TypeScript exports",
      },
    ],
    overview: { task: "Verify sticky scroll across long conversations" },
  },
  {
    id: "child",
    name: "Mobile reviewer",
    status: "running",
    inFlight: true,
    parentId: "running",
    role: "reviewer",
    overview: { task: "Check 320px composition and long bilingual titles" },
  },
  {
    id: "paused",
    name: "Package verification",
    status: "paused",
    autoWake: false,
    overview: { task: "Wait for the integration branch" },
  },
  {
    id: "none",
    name: "Queue recovery",
    status: "idle",
    overview: { task: "Preserve drafts after reconnect" },
  },
  {
    id: "unread",
    name: "Новые результаты проверки доступности интерфейса",
    status: "completed",
    threadId: "thread-result",
    lastCompletedTurn: "turn-result",
    lastCompletedTurnStatus: "completed",
    overview: {
      task: "Accessibility review",
      result: "Keyboard focus, accessible names, and contrast checks passed.",
    },
  },
  {
    id: "finished",
    name: "Storage adapter",
    status: "completed",
    overview: {
      task: "Inspect request identities",
      result: "Request identities remain stable across retries.",
    },
  },
  ...Array.from({ length: 12 }, (_, index): Agent => ({
    id: `done-${index}`,
    name: `Completed review ${index + 1}`,
    status: "completed",
  })),
];
const workers: Agent[] = workerFixtures.map((agent) => ({
  ...agent,
  source: "managed",
  rootId: lead.id,
  parentId: agent.parentId || lead.id,
  model: agent.role === "reviewer" ? "opus" : "gpt-6-luna",
  provider: agent.role === "reviewer" ? "claude" : "codex",
  created: now - 180,
  updated: now - 20,
  effort: "medium",
}));
const threads = [lead, ...workers];
const snapshot: Snapshot = {
  token: "fixture",
  stateDir: "storybook",
  threads,
  nodes: threads,
  chats: [],
  edges: [],
  runtime: {
    agents: threads,
    rooms: [],
    tasks: [],
    monitors: [],
    complaints: [],
    requests: [{ id: "request", agent: "question", status: "pending" }],
    rules: [],
    projects: [],
    peerTeams: [],
    events: [],
    work: [],
  },
};
const indicators = chatIndicators(snapshot);

const meta = {
  title: "Agents/Team dispatch",
  component: TeamPanel,
  parameters: { layout: "fullscreen" },
  args: {
    lead,
    workers,
    accounts: [],
    opened: "lead",
    answerIds: new Set(["question"]),
    deferredIds: new Set<string>(),
    indicators,
    workerQuery: "",
    setWorkerQuery: fn(),
    workerFilter: "all",
    setWorkerFilter: fn(),
    completedOpen: false,
    setCompletedOpen: fn(),
    setTeamOpen: fn(),
    setWideTeamOpen: fn(),
    saveClosed: fn(),
    open: fn(),
    previewResult: fn(),
    remove: fn(),
  },
} satisfies Meta<typeof TeamPanel>;
export default meta;
type Story = StoryObj<typeof meta>;
export const Dispatch: Story = {
  render: (args) => {
    const [query, setQuery] = useState("");
    const [filter, setFilter] = useState("all");
    const [completed, setCompleted] = useState(false);
    return (
      <div
        style={{
          height: "100dvh",
          margin: "calc(-1 * var(--mantine-spacing-lg))",
          display: "flex",
          justifyContent: "end",
          minWidth: 0,
        }}
      >
        <TeamPanel
          {...args}
          workerQuery={query}
          setWorkerQuery={setQuery}
          workerFilter={filter}
          setWorkerFilter={setFilter}
          completedOpen={completed}
          setCompletedOpen={setCompleted}
        />
      </div>
    );
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(
      canvas.getByRole("complementary", { name: "Team" }),
    ).toBeVisible();
    const failed = canvas.getByRole("region", { name: "Failed" });
    await expect(
      within(failed).getByText("Type check failed in the project adapter.", {
        selector: ".worker-error",
      }),
    ).toBeVisible();
    await expect(
      canvas.getAllByRole("img", { name: /^Reviewer$/ }).length,
    ).toBeGreaterThan(0);
    await expect(
      canvas.getAllByRole("img", { name: /^Codex$/ }).length,
    ).toBeGreaterThan(0);
    await expect(
      canvasElement.querySelector(
        '[data-worker="running"] [data-chat-status="working"]',
      ),
    ).toBeVisible();
    await expect(
      canvas.queryByText("No final report available"),
    ).not.toBeInTheDocument();
    await userEvent.click(canvas.getByRole("button", { name: /^Failed$/ }));
    await expect(
      canvas.getByRole("button", { name: /^Failed$/ }),
    ).toHaveAttribute("aria-pressed", "true");
    await expect(
      canvas.queryByText("Transcript worker"),
    ).not.toBeInTheDocument();
    await expect(canvasElement.querySelectorAll("[data-worker]")).toHaveLength(
      1,
    );
    await userEvent.click(canvas.getByRole("button", { name: /^All$/ }));
    await expect(
      canvasElement.querySelector('[data-worker="running"]'),
    ).toBeVisible();
    const progress = canvasElement.querySelector(".worker-command-details")!;
    await expect(progress).not.toHaveAttribute("open");
    await userEvent.click(progress.querySelector("summary")!);
    await expect(
      within(progress as HTMLElement).getByText("Checking TypeScript exports"),
    ).toBeVisible();
    await userEvent.click(progress.querySelector("summary")!);
    await expect(canvas.queryByText("Storage adapter")).not.toBeInTheDocument();
    await userEvent.click(canvas.getByLabelText("Finished (14)"));
    await expect(canvas.getByText("Storage adapter")).toBeVisible();
    await userEvent.type(
      canvas.getByRole("searchbox", { name: "Find a subagent" }),
      "Новые результаты",
    );
    await expect(
      canvas.getByText("Новые результаты проверки доступности интерфейса"),
    ).toBeVisible();
    await userEvent.clear(
      canvas.getByRole("searchbox", { name: "Find a subagent" }),
    );
    await userEvent.click(canvas.getByLabelText("Finished (14)"));
  },
};
export const AfUtils: Story = {
  ...Dispatch,
  args: {
    ...meta.args,
    lead: { ...lead, name: "af-utils / Release audit" },
    workers: workers.map((agent) => ({
      ...agent,
      name:
        agent.id === "running"
          ? "Investigate timeout recovery in the release verification pipeline"
          : agent.name,
    })),
  },
};
export const Attar: Story = {
  ...Dispatch,
  args: {
    ...meta.args,
    lead: { ...lead, name: "attar / Deployment preparation" },
  },
};

/** Screenshot fixture leaves interaction state untouched. */
export const Overview: Story = { ...Dispatch, play: undefined };

export const Workspace: Story = {
  render: (args) => (
    <div
      className="workspace-frame"
      style={
        {
          height: "100dvh",
          margin: "calc(-1 * var(--mantine-spacing-lg))",
          "--studio-content-width-ratio": 0.6,
        } as import("react").CSSProperties
      }
    >
      <main
        className="workspace"
        style={{ padding: "var(--mantine-spacing-lg)" }}
      >
        <h2 style={{ fontSize: "var(--studio-text-title)", fontWeight: 600 }}>
          Studio release review
        </h2>
        <p style={{ color: "var(--muted)" }}>
          Team activity stays beside the conversation when the content
          preference leaves enough room.
        </p>
      </main>
      <TeamPanel {...args} />
    </div>
  ),
};

export const ImplicitLead: Story = {
  args: {
    opened: "running",
    lead: {
      ...lead,
      overview: {
        result: "Release verification passed.",
        resultTruncated: true,
        resultFile: "reports/release.md",
      },
    },
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await expect(
      canvasElement.querySelector("[data-lead]"),
    ).not.toBeInTheDocument();
    const root = canvasElement
      .querySelector('[data-worker="running"]')!
      .closest(".worker-entry")!;
    const child = canvasElement
      .querySelector('[data-worker="child"]')!
      .closest(".worker-entry")!;
    await expect(
      (root as HTMLElement).style.getPropertyValue("--worker-depth"),
    ).toBe("0");
    await expect(
      (child as HTMLElement).style.getPropertyValue("--worker-depth"),
    ).toBe("1");
    await userEvent.click(
      canvas.getByRole("button", { name: "Back to main agent" }),
    );
    await expect(args.open).toHaveBeenLastCalledWith("lead");
    await userEvent.click(canvas.getByText("Main agent report"));
    await expect(
      canvas.getByText("Release verification passed."),
    ).toBeVisible();
    await userEvent.click(
      canvas.getByRole("button", { name: "Continue in main chat" }),
    );
    await expect(args.open).toHaveBeenLastCalledWith("lead");
    await userEvent.click(
      canvas.getByRole("link", { name: "Open main agent submitted result" }),
    );
    await expect(args.previewResult).toHaveBeenLastCalledWith({
      agent: "lead",
      path: "reports/release.md",
    });
  },
};
