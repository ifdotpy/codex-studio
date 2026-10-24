import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import { BackgroundTaskStrip } from "./BackgroundTasks";
import type { DisplayBackgroundTask } from "../backgroundTaskModel";
const task: DisplayBackgroundTask = {
  id: "monitor",
  agent: "worker",
  kind: "monitor",
  created: 1700000000,
  status: "running",
  command: "pnpm --filter codex-agents-web run typecheck:fast",
};
const meta = {
  title: "Agents/Background activity",
  component: BackgroundTaskStrip,
  args: {
    task,
    now: 1700000195,
    selected: false,
    select: fn(),
    owner: { id: "worker", name: "Release reviewer" },
  },
  decorators: [
    (Story) => (
      <div
        className="tasks-drawer"
        style={{ width: "min(var(--studio-team-size), 100%)" }}
      >
        <Story />
      </div>
    ),
  ],
} satisfies Meta<typeof BackgroundTaskStrip>;
export default meta;
type Story = StoryObj<typeof meta>;
export const Monitor: Story = {
  play: async ({ canvasElement, args }) => {
    await userEvent.click(within(canvasElement).getByRole("button"));
    await expect(args.select).toHaveBeenCalledOnce();
  },
};
export const Approval: Story = {
  args: { task: { ...task, status: "approval" } },
};
export const Tool: Story = {
  args: { task: { ...task, kind: "tool", command: null, name: "webSearch" } },
};
export const UnknownOutcome: Story = {
  args: { task: { ...task, status: "lost" } },
};
