import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import MessageActions from "./MessageActions";

const meta = {
  title: "Conversation/Transcript/Message actions",
  component: MessageActions,
} satisfies Meta<typeof MessageActions>;

export default meta;
type Story = StoryObj<typeof meta>;

export const CopyOnly: Story = {
  args: { onCopy: fn() },
  play: async ({ canvasElement, args }) => {
    await userEvent.click(
      within(canvasElement).getByRole("button", { name: "Copy message" }),
    );
    await expect(args.onCopy).toHaveBeenCalledOnce();
  },
};

export const FullTranscriptActions: Story = {
  args: {
    onCopy: fn(),
    onQuote: fn(),
    onEdit: fn(),
    onBranch: fn(),
    onAnotherAnswer: fn(),
  },
};

export const EditInProgress: Story = {
  args: {
    onCopy: fn(),
    onQuote: fn(),
    onEdit: fn(),
    editLoading: true,
    onBranch: fn(),
    branchDisabled: true,
  },
};
