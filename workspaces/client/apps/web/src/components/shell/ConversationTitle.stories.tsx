import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import ConversationTitle from "./ConversationTitle";
import type { Agent } from "../../types";

const lead = {
  id: "lead-1",
  name: "Main agent",
  source: "managed",
  status: "running",
} as Agent;

const meta = {
  title: "Shell/Conversation title",
  component: ConversationTitle,
} satisfies Meta<typeof ConversationTitle>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ActiveConversation: Story = {
  args: {
    title: "Refactor the conversation view",
    agent: lead,
    statusText: "Working",
  },
  play: async ({ canvasElement }) => {
    expect(
      within(canvasElement).getByRole("heading", { level: 1 }),
    ).toHaveTextContent("Refactor the conversation view");
  },
};

export const AgentConversation: Story = {
  args: {
    ...ActiveConversation.args,
    title: "Build transcript components",
    projectPrefix: "studio · ",
    onBack: fn(),
  },
  play: async ({ canvasElement, args }) => {
    await userEvent.click(
      within(canvasElement).getByRole("button", { name: "Back to main agent" }),
    );
    await expect(args.onBack).toHaveBeenCalledOnce();
  },
};
