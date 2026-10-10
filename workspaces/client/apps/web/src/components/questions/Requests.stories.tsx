import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, userEvent, within } from "storybook/test";
import Requests from "./Requests";
import type { Agent } from "../../types";
import type { components } from "../../generated/api";

type RequestDto = components["schemas"]["RequestEntityDto"];

const agent = {
  kind: "agent",
  id: "worker-1",
  name: "Release worker",
  source: "managed",
  status: "approval",
  model: "gpt-6",
  created: 1,
} satisfies Agent;

const question: RequestDto = {
  id: "question-1",
  agent: agent.id,
  method: "agent/asyncQuestion",
  status: "pending",
  createdAt: 1,
  params: {
    questions: [
      {
        id: "scope",
        question: "Which files should I include?",
        options: [
          { label: "One file", description: "Keep the change focused." },
          { label: "All files" },
        ],
      },
    ],
  },
};

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

export const DeferredQuestion: Story = {
  args: {
    requests: [{ ...question, deferred: true }],
    allRequests: [{ ...question, deferred: true }],
  },
};
