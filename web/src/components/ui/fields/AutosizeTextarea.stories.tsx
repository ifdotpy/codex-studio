import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect } from "storybook/test";
import { AutosizeTextarea } from "./AutosizeTextarea";

const meta = {
  title: "UI/Fields/AutosizeTextarea",
  component: AutosizeTextarea,
  args: {
    label: "Message",
    placeholder: "Write a message",
  },
} satisfies Meta<typeof AutosizeTextarea>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {
  play: async ({ canvasElement, userEvent }) => {
    const field = canvasElement.querySelector<HTMLTextAreaElement>("textarea")!;
    const initialHeight = Number.parseFloat(field.style.height);
    await userEvent.type(field, "Two lines{Enter}stay together");
    await expect(field).toHaveValue("Two lines\nstay together");
    await expect(Number.parseFloat(field.style.height)).toBeGreaterThan(
      initialHeight,
    );
  },
};

export const CompactRows: Story = {
  args: { maxRows: 3, defaultValue: "First line\nSecond line" },
};
