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
    serverAlias: "MAC",
  },
  play: async ({ canvasElement }) => {
    expect(
      within(canvasElement).getByRole("heading", { level: 1 }),
    ).toHaveTextContent("Refactor the conversation view");
    await expect(
      canvasElement.querySelector("#conversation-status"),
    ).toBeNull();
  },
};

export const AgentConversation: Story = {
  args: {
    ...ActiveConversation.args,
    title: "Build transcript components",
    onBack: fn(),
  },
  play: async ({ canvasElement, args }) => {
    await userEvent.click(
      within(canvasElement).getByRole("button", { name: "Back to main agent" }),
    );
    await expect(args.onBack).toHaveBeenCalledOnce();
  },
};

export const OfflineServer: Story = {
  args: { ...ActiveConversation.args, serverOffline: true },
  play: async ({ canvasElement }) => {
    const server = within(canvasElement).getByLabelText("MAC offline");
    await expect(server).toBeVisible();
    const line = server.querySelector(".chat-server-line")!;
    const reference = document.createElement("span");
    reference.style.color = "var(--studio-state-error)";
    server.append(reference);
    await expect(getComputedStyle(line).color).toBe(
      getComputedStyle(reference).color,
    );
    reference.remove();
  },
};

export const WaitingConversation: Story = {
  args: {
    ...ActiveConversation.args,
    agent: {
      ...lead,
      status: "waiting",
      inFlight: false,
      parkedEvent: "monitor_exit",
    },
  },
  play: async ({ canvasElement }) => {
    await expect(canvasElement).not.toHaveTextContent(
      /Waiting|Working|monitor/i,
    );
    await expect(
      within(canvasElement).getByRole("heading", { level: 1 }),
    ).toBeVisible();
  },
};
