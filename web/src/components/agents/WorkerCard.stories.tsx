import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import WorkerCard from "./WorkerCard";
import type { Agent } from "../../types";

const agent = {
  id: "worker-1",
  name: "Transcript worker",
  source: "managed",
  status: "running",
  model: "gpt-6",
  created: 1,
  role: "worker",
  overview: {
    task: "Extract reusable transcript presentation components.",
    result: "The story renders the same typed card used by the team panel.",
  },
} as Agent;

const meta = {
  title: "Agents/Worker card",
  component: WorkerCard,
} satisfies Meta<typeof WorkerCard>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Working: Story = {
  args: {
    agent,
    selected: false,
    awaitingAnswer: false,
    deferred: false,
    open: fn(),
    previewResult: fn(),
  },
  play: async ({ canvasElement, args }) => {
    await userEvent.click(
      within(canvasElement).getByRole("button", { name: /Transcript worker/ }),
    );
    await expect(args.open).toHaveBeenCalledOnce();
  },
};

export const SelectedNeedsAnswer: Story = {
  args: {
    agent,
    selected: true,
    awaitingAnswer: true,
    deferred: false,
    open: fn(),
    previewResult: fn(),
    remove: fn(),
  },
};
