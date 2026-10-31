import { AgentSource, AgentStatus } from "../../generated/api";
import { approval } from "../../approvalFixtures";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, userEvent, within } from "storybook/test";
import Requests from "./Requests";
import type { Agent } from "../../types";

const agent = {
  kind: "agent",
  id: "worker-1",
  name: "Release worker",
  source: AgentSource.managed,
  status: AgentStatus.approval,
  model: "gpt-6",
  created: 1,
} satisfies Agent;

const question = approval({
  id: "question-1",
  agent: agent.id,
  request: {
    kind: "agent_question",
    questions: [
      {
        id: "scope",
        question: "Which files should I include?",
        header: null,
        options: [
          {
            label: "One file",
            description: "Keep the change focused.",
            preview: null,
          },
          { label: "All files", description: null, preview: null },
        ],
        is_secret: false,
        multi_select: null,
        is_other: null,
      },
    ],
  },
});

const meta = {
  title: "Questions/Requests",
  component: Requests,
  args: {
    requests: [question],
    allRequests: [question],
    scope: "storybook",
    agents: [agent],
    refresh: async () => {},
    notify: () => {},
  },
} satisfies Meta<typeof Requests>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ActiveQuestion: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByRole("button", { name: "Answer" }));
    expect(
      canvas.getByRole("textbox", { name: "Which files should I include?" }),
    ).toBeVisible();
    await userEvent.click(canvas.getByRole("button", { name: /One file/ }));
    expect(canvas.getByRole("button", { name: /One file/ })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
  },
};

const deferredQuestion = {
  ...question,
  deferral: { active: true, at: 2, by: { kind: "owner" as const } },
};

export const DeferredQuestion: Story = {
  args: {
    requests: [deferredQuestion],
    allRequests: [deferredQuestion],
  },
};

const sending = {
  ...question,
  state: {
    kind: "answering" as const,
    answer: {
      kind: "answers" as const,
      answers: [{ id: "scope", answer: ["One file"] }],
    },
    command: "command",
  },
};

/** The answer is stored and on its way; the card shows no controls. */
export const AnsweringQuestion: Story = {
  args: { requests: [sending], allRequests: [sending] },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    expect(canvas.getByText("Sending your answer")).toBeVisible();
    expect(canvas.queryByRole("button", { name: "Answer" })).toBeNull();
  },
};

const approveCommand = approval({
  id: "command-1",
  agent: agent.id,
  request: {
    kind: "exec",
    command: "npm run build",
    cwd: "/work/app",
    reason: "Build before the release",
  },
});

export const CommandApproval: Story = {
  args: { requests: [approveCommand], allRequests: [approveCommand] },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    expect(canvas.getByText("npm run build")).toBeVisible();
    expect(canvas.getByRole("button", { name: "Approve" })).toBeVisible();
  },
};
