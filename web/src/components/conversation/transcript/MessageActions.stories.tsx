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

export const AssistantActionTooltips: Story = {
  args: {
    onCopy: fn(),
    onQuote: fn(),
    onBranch: fn(),
    onAnotherAnswer: fn(),
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const documentBody = within(canvasElement.ownerDocument.body);
    const actions = [
      ["Copy message", "Copy this message"],
      ["Quote message", "Quote this message in your reply"],
      ["Branch after this turn", "Start a new branch after this turn"],
      ["Another answer in a new chat", "Ask for another answer in a new chat"],
    ] as const;

    for (const [name, label] of actions) {
      const button = canvas.getByRole("button", { name });
      await userEvent.hover(button);
      await expect(
        await documentBody.findByRole("tooltip", { name: label }),
      ).toHaveTextContent(label);
      await userEvent.unhover(button);
    }

    const copy = canvas.getByRole("button", { name: "Copy message" });
    copy.focus();
    for (const [index, [name, label]] of actions.entries()) {
      if (index > 0) await userEvent.tab();
      const button = canvas.getByRole("button", { name });
      await expect(button).toHaveFocus();
      await expect(
        await documentBody.findByRole("tooltip", { name: label }),
      ).toHaveTextContent(label);
    }
  },
};

export const EditInProgress: Story = {
  args: {
    onCopy: fn(),
    onQuote: fn(),
    onEdit: fn(),
    editDisabled: true,
    onBranch: fn(),
    branchDisabled: true,
    onAnotherAnswer: fn(),
    anotherAnswerDisabled: true,
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const documentBody = within(canvasElement.ownerDocument.body);
    const actions = [
      ["Edit in a new chat", "Edit this message in a new chat"],
      ["Branch after this turn", "Start a new branch after this turn"],
      ["Another answer in a new chat", "Ask for another answer in a new chat"],
    ] as const;

    for (const [name, label] of actions) {
      const target = canvas.getByRole("group", { name });
      await userEvent.hover(target);
      await expect(
        await documentBody.findByRole("tooltip", { name: label }),
      ).toHaveTextContent(label);
      await userEvent.unhover(target);
      target.focus();
      await expect(target).toHaveFocus();
      await expect(
        await documentBody.findByRole("tooltip", { name: label }),
      ).toHaveTextContent(label);
    }
  },
};
