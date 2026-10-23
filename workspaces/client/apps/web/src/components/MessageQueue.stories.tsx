import type { Meta, StoryObj } from "@storybook/react-vite";
import { fn } from "storybook/test";
import MessageQueue from "./MessageQueue";
const meta = {
  title: "Agents/Message queue",
  component: MessageQueue,
  args: {
    scope: "storybook-queue",
    items: [
      {
        id: "queued-1",
        text: "Review the request identity before the next turn.",
        created: 1700000000,
        status: "pending",
        kind: "user",
      },
      {
        id: "queued-2",
        text: "Проверьте восстановление длинных черновиков после переподключения к проекту.",
        created: 1700000010,
        status: "pending",
        kind: "user",
      },
    ],
    onEdit: fn(),
    onCancel: fn(),
    onReorder: fn(),
    onSendNow: fn(),
    canReorder: true,
  },
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof MessageQueue>;
export default meta;
type Story = StoryObj<typeof meta>;
export const Queued: Story = {};
export const Checking: Story = { args: { loading: true } };
export const DeliveryError: Story = {
  args: {
    error: "Queue could not be refreshed. Saved edits remain available.",
  },
};
