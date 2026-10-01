import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect } from "storybook/test";
import AgentAvatar from "./AgentAvatar";

const meta = {
  title: "Agents/Avatar",
  component: AgentAvatar,
} satisfies Meta<typeof AgentAvatar>;

export default meta;
type Story = StoryObj<typeof meta>;

export const StableIdentity: Story = {
  args: { id: "agent-42", size: 32 },
  play: async ({ canvasElement }) => {
    const avatar = canvasElement.querySelector("svg.agent-avatar");
    expect(avatar).not.toBeNull();
    expect(avatar).toHaveAttribute("width", "32");
    expect(avatar).toHaveAttribute("height", "32");
  },
};

export const Compact: Story = {
  args: { id: "agent-42", size: 24 },
};
