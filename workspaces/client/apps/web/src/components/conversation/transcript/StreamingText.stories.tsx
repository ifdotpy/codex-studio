import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, within } from "storybook/test";
import StreamingText from "./StreamingText";

const meta = {
  title: "Conversation/Transcript/Markdown",
  component: StreamingText,
} satisfies Meta<typeof StreamingText>;

export default meta;
type Story = StoryObj<typeof meta>;

export const CompletedReply: Story = {
  args: {
    text: "## Release summary\n\nThe changes are ready.\n\n- typed transcript actions\n- reusable question fields",
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    expect(
      canvas.getByRole("heading", { name: "Release summary" }),
    ).toBeVisible();
    expect(
      canvas.getByText("typed transcript actions", { exact: true }),
    ).toBeVisible();
  },
};

export const StreamingReply: Story = {
  args: {
    text: "The response is still arriving.",
    streaming: true,
  },
  play: async ({ canvasElement }) => {
    expect(
      within(canvasElement).getByText("The response is still arriving."),
    ).toBeVisible();
  },
};
