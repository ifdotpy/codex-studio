import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, within } from "storybook/test";
import MessageDate from "./MessageDate";

const meta = {
  title: "Conversation/Transcript/Message date",
  component: MessageDate,
} satisfies Meta<typeof MessageDate>;

export default meta;
type Story = StoryObj<typeof meta>;

export const RecordedTimestamp: Story = {
  args: { at: "2026-09-30T12:00:00.000Z" },
  play: async ({ canvasElement }) => {
    const time = canvasElement.querySelector("time");
    expect(time).toHaveAttribute("datetime", "2026-09-30T12:00:00.000Z");
    expect(time).not.toBeEmptyDOMElement();
  },
};

export const MissingTimestamp: Story = {
  args: { at: null },
  play: async ({ canvasElement }) => {
    expect(within(canvasElement).getByText("Date unavailable")).toBeVisible();
  },
};
