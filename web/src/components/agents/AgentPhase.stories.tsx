import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, within } from "storybook/test";
import AgentPhase from "./AgentPhase";
import type { Agent } from "../../types";

const meta = {
  title: "Agents/Status",
  component: AgentPhase,
  args: { connection: "connected" },
} satisfies Meta<typeof AgentPhase>;

export default meta;
type Story = StoryObj<typeof meta>;

const thinkingAgent = {
  id: "worker-1",
  name: "Transcript worker",
  source: "managed",
  model: "gpt-6",
  status: "running",
  provider: "codex",
  created: 1,
  activity: { phase: "thinking" },
  inFlight: true,
} as Agent;

export const Thinking: Story = {
  args: { agent: thinkingAgent },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    expect(canvas.getByText("Thinking", { exact: true })).toBeVisible();
  },
};

export const WaitingForAnswer: Story = {
  args: {
    agent: {
      ...thinkingAgent,
      status: "approval",
      activity: undefined,
      inFlight: false,
      provider: "claude",
      model: "claude-sonnet",
    },
  },
  play: async ({ canvasElement }) => {
    expect(
      within(canvasElement).getByText("Waiting for your answer", {
        exact: true,
      }),
    ).toBeVisible();
  },
};

export const Reconnecting: Story = {
  args: {
    agent: { ...thinkingAgent, activity: { phase: "tool" } },
    connection: "reconnecting",
  },
  play: async ({ canvasElement }) => {
    expect(
      within(canvasElement).getByText("Connection lost. Reconnecting…", {
        exact: true,
      }),
    ).toBeVisible();
  },
};
