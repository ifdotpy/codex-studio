import {
  AgentRole,
  AgentSource,
  AgentStatus,
  RequestStatus,
} from "../../generated/api";
import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, waitFor, within } from "storybook/test";
import type { Agent, Snapshot } from "../../types";
import type { Account } from "../Accounts";
import { chatIndicators } from "../chat-status/chatStatusModel";
import TeamPanel from "./TeamPanel";

const now = Math.floor(Date.now() / 1000);
const lead: Agent = {
  id: "lead",
  name: "Studio release",
  role: AgentRole.lead,
  isLead: true,
  source: AgentSource.managed,
  status: AgentStatus.running,
  inFlight: true,
  model: "gpt-6-astra",
  created: now - 195,
  activity: { phase: "thinking", at: now - 32 },
};
const workerFixtures: Agent[] = [
  {
    id: "question",
    name: "Permissions review",
    status: AgentStatus.approval,
    role: AgentRole.reviewer,
    overview: { task: "Confirm the allowed command scope" },
  },
  {
    id: "error",
    name: "Обновление индексов для нескольких проектов и проверка восстановления после перезапуска",
    status: AgentStatus.failed,
    error: "Type check failed in the project adapter.",
    overview: { task: "Repair the project migration" },
  },
  {
    id: "running",
    name: "Transcript worker",
    status: AgentStatus.running,
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
    status: AgentStatus.running,
    inFlight: true,
    parentId: "running",
    role: AgentRole.reviewer,
    overview: { task: "Check 320px composition and long bilingual titles" },
  },
  {
    id: "paused",
    name: "Package verification",
    status: AgentStatus.paused,
    autoWake: false,
    overview: { task: "Wait for the integration branch" },
  },
  {
    id: "none",
    name: "Queue recovery",
    status: AgentStatus.idle,
    overview: { task: "Preserve drafts after reconnect" },
  },
  {
    id: "unread",
    name: "Новые результаты проверки доступности интерфейса",
    status: AgentStatus.completed,
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
    status: AgentStatus.completed,
    overview: {
      task: "Inspect request identities",
      result: "Request identities remain stable across retries.",
    },
  },
  ...Array.from({ length: 12 }, (_, index): Agent => ({
    id: `done-${index}`,
    name: `Completed review ${index + 1}`,
    status: AgentStatus.completed,
  })),
];
const workers: Agent[] = workerFixtures.map((agent) => ({
  ...agent,
  source: "managed",
  rootId: lead.id,
  parentId: agent.parentId || lead.id,
  model: agent.role === AgentRole.reviewer ? "opus" : "gpt-6-luna",
  provider: agent.role === AgentRole.reviewer ? "claude" : "codex",
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
    requests: [
      { id: "request", agent: "question", status: RequestStatus.pending },
    ],
    rules: [],
    projects: [],
    peerTeams: [],
    events: [],
    work: [],
    studioErrors: [],
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
    for (const worker of canvasElement.querySelectorAll(".worker-entry"))
      await expect(getComputedStyle(worker).boxShadow).toBe("none");
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
    await expect(progress).not.toHaveAttribute("data-expanded");
    await userEvent.click(progress.querySelector(".disclosure-control")!);
    await waitFor(() =>
      expect(
        within(progress as HTMLElement).getByText(
          "Checking TypeScript exports",
        ),
      ).toBeVisible(),
    );
    await userEvent.click(progress.querySelector(".disclosure-control")!);
    await expect(canvas.queryByText("Storage adapter")).not.toBeInTheDocument();
    await userEvent.click(canvas.getByLabelText("Finished (14)"));
    // The group opens through an animated Mantine Collapse.
    await waitFor(() =>
      expect(canvas.getByText("Storage adapter")).toBeVisible(),
    );
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
const { play: _play, ...overview } = Dispatch;
export const Overview: Story = overview;

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
    await waitFor(() =>
      expect(canvas.getByText("Release verification passed.")).toBeVisible(),
    );
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

export const SharedGutter: Story = {
  play: async ({ canvasElement }) => {
    const panel = canvasElement.querySelector("#team")!;
    const computed = getComputedStyle(panel);
    const gutterProbe = document.createElement("div");
    gutterProbe.style.width = "var(--mantine-spacing-md)";
    panel.append(gutterProbe);
    const gutter = gutterProbe.getBoundingClientRect().width;
    gutterProbe.remove();
    await expect(parseFloat(computed.paddingInlineStart)).toBe(gutter);
    const edge =
      panel.getBoundingClientRect().left +
      parseFloat(computed.borderInlineStartWidth) +
      gutter;
    for (const selector of [
      ".team-heading h2",
      ".team-summary-filters",
      // Only the panel's own search; nested budget fields sit deeper.
      ":scope > .mantine-TextInput-root",
      ".team-status-group h3",
      ".worker-group > .disclosure-control",
      ".worker-entry:not([data-has-parent])",
    ]) {
      for (const element of panel.querySelectorAll(selector)) {
        await expect(element.getBoundingClientRect().left).toBeCloseTo(edge, 0);
      }
    }
  },
};

export const ComfortableStrips: Story = {
  play: async ({ canvasElement }) => {
    const panel = canvasElement.querySelector("#team")!;
    const step = stripStep(panel);
    for (const strip of panel.querySelectorAll<HTMLElement>(".worker-entry")) {
      const style = getComputedStyle(strip);
      for (const padding of [
        style.paddingInlineStart,
        style.paddingBlockStart,
        style.paddingBlockEnd,
      ])
        await expect(parseFloat(padding)).toBeCloseTo(step, 1);
      const name = strip
        .querySelector(".worker-name-line")!
        .getBoundingClientRect();
      const activity = strip
        .querySelector(".worker-live-line")!
        .getBoundingClientRect();
      const model = strip
        .querySelector(".worker-bottom")!
        .getBoundingClientRect();
      await expect(activity.top - name.bottom).toBeGreaterThan(0);
      await expect(model.top - activity.bottom).toBeGreaterThan(0);
    }
  },
};

/** Width of the Team strip spacing step, in CSS pixels. */
function stripStep(panel: Element) {
  const probe = document.createElement("div");
  probe.style.width = "var(--studio-strip-step)";
  panel.append(probe);
  const step = probe.getBoundingClientRect().width;
  probe.remove();
  return step;
}

const rowAccounts = [
  { id: "default", label: "Personal" },
  { id: "personal-2", label: "Personal account with a long label" },
].map(
  (account) =>
    ({
      ...account,
      home: "/storybook",
      source: "storybook",
      provider: "codex",
      status: "ready",
    }) as Account,
);
const rowTask =
  "Own task work-1792784011234567890-17: make the Team panel rows fit the Duo drawer and the desktop dock.";
const rowFixtures: (Pick<Agent, "id" | "name"> & Partial<Agent>)[] = [
  {
    id: "long",
    name: "duo-ios-shell-and-bonjour-discovery",
    commandProgress: [
      {
        monitorId: "long-command",
        stage:
          "sleep 3; cd /tmp/duo-ios-shell && pnpm exec playwright test --project=client tests/teams/team-panel-ui.spec.ts",
        startedAt: now - 2652,
        lastOutputLine: "",
      },
    ],
    overview: { task: rowTask, resultFile: "reports/duo-ios-shell.md" },
  },
  {
    id: "short",
    name: "chat-fixes",
    activity: { phase: "thinking", at: now - 71 },
    environment: "linux",
    overview: { task: rowTask },
  },
  {
    id: "wide",
    name: "release-verification-pipeline-audit",
    activity: { phase: "tool", at: now - 12 },
    commandProgress: [
      {
        monitorId: "wide-command",
        stage: "cd /Users/igor/Projects/codex-agents && just test-fast",
        startedAt: now - 12,
        lastOutputLine: "",
      },
    ],
    overview: { task: "Short task", resultFile: "reports/wide.md" },
  },
];
const rowWorkers: Agent[] = rowFixtures.map((agent) => ({
  ...agent,
  source: "managed",
  status: "running",
  inFlight: true,
  rootId: lead.id,
  parentId: lead.id,
  provider: "codex",
  model: "gpt-6-luna",
  effort: "high",
  accountKey: "personal-2",
  created: now - 4229 * 60,
}));

/** Rows at one Team panel width, with long names, commands and tasks. */
function rowStory(width: number): Story {
  return {
    args: { workers: rowWorkers, accounts: rowAccounts },
    render: (args) => (
      <div
        style={
          {
            height: "100dvh",
            width: `${width}px`,
            display: "flex",
            "--studio-team-size": `${width}px`,
          } as import("react").CSSProperties
        }
      >
        <TeamPanel {...args} />
      </div>
    ),
    play: async ({ canvasElement }) => {
      // Rows use content-visibility: auto; measure after their first rendered frame.
      for (let frame = 0; frame < 2; frame++)
        await new Promise((resolve) => requestAnimationFrame(resolve));
      const panel = canvasElement.querySelector("#team")!;
      await expect(panel.getBoundingClientRect().width).toBeCloseTo(width, 0);
      await expect(panel.scrollWidth).toBeLessThanOrEqual(panel.clientWidth);
      const step = stripStep(panel);
      const rows = panel.querySelectorAll<HTMLElement>(".worker-entry");
      await expect(rows).toHaveLength(rowWorkers.length);
      for (const row of rows) {
        const box = row.getBoundingClientRect();
        const nameLine = row
          .querySelector(".worker-name-line")!
          .getBoundingClientRect();
        // Line 1: the first 16 name characters stay visible.
        const strong = row.querySelector("strong")!;
        const range = document.createRange();
        range.setStart(strong.firstChild!, 0);
        range.setEnd(
          strong.firstChild!,
          Math.min(16, strong.textContent!.length),
        );
        await expect(range.getBoundingClientRect().width).toBeLessThanOrEqual(
          strong.clientWidth + 1,
        );
        // Line 1: elapsed time is right aligned.
        await expect(
          row.querySelector(".worker-elapsed")!.getBoundingClientRect().right,
        ).toBeCloseTo(nameLine.right, 0);
        // Line 2: one activity line; the task is on its own line below.
        const live = row.querySelector(".worker-live-line")!;
        await expect(live.querySelector(".worker-task")).toBeNull();
        await expect(live.getBoundingClientRect().height).toBeLessThan(
          parseFloat(getComputedStyle(live).lineHeight) * 1.5,
        );
        const task = row.querySelector(".worker-task")!;
        await expect(task.getBoundingClientRect().top).toBeGreaterThanOrEqual(
          live.getBoundingClientRect().bottom,
        );
        // Line 3: meta items wrap whole; the account drops when narrow.
        for (const item of row.querySelectorAll<HTMLElement>(
          ".worker-meta > *",
        ))
          if (item.getClientRects().length)
            await expect(item.scrollWidth).toBeLessThanOrEqual(
              item.clientWidth + 1,
            );
        await expect(
          row.querySelector(".worker-account")!.getClientRects().length > 0,
        ).toBe(width > 320);
        // The divider spans the content column; padding is on the 8 px step.
        const after = getComputedStyle(row, "::after");
        await expect(box.left + parseFloat(after.left)).toBeCloseTo(
          nameLine.left,
          0,
        );
        await expect(parseFloat(after.right)).toBe(0);
        const style = getComputedStyle(row);
        for (const padding of [
          style.paddingInlineStart,
          style.paddingBlockStart,
          style.paddingBlockEnd,
        ])
          await expect(parseFloat(padding)).toBeCloseTo(step, 1);
        // The result button sits inside the row on the content column.
        const result = row.querySelector(".worker-result");
        if (result) {
          const button = result.getBoundingClientRect();
          await expect(button.left).toBeCloseTo(nameLine.left, 0);
          await expect(button.bottom).toBeLessThanOrEqual(box.bottom);
        }
      }
    },
  };
}
export const Rows320 = rowStory(320);
export const Rows470 = rowStory(470);
export const Rows600 = rowStory(600);
/** Long rows in the default desktop dock beside the conversation. */
export const RowsWorkspace: Story = {
  ...Workspace,
  args: { workers: rowWorkers, accounts: rowAccounts },
};
