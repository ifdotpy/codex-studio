import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, within } from "storybook/test";
import ComposerAttachments, { type Attachment } from "./ComposerAttachments";

const sampleAttachment: Attachment = {
  id: "storybook-attachment",
  name: "brief.txt",
  mime: "text/plain",
  image: false,
  size: 84,
};

const meta = {
  title: "Composer/Attachments",
  component: ComposerAttachments,
  args: {
    notify: fn(),
    assets: [],
    uploading: false,
    disabled: false,
    add: fn(async () => undefined),
    remove: fn(),
  },
} satisfies Meta<typeof ComposerAttachments>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ChooseFile: Story = {
  play: async ({ args, canvasElement, userEvent }) => {
    const input =
      canvasElement.querySelector<HTMLInputElement>('input[type="file"]')!;
    await userEvent.upload(
      input,
      new File(["Storybook fixture"], "brief.txt", { type: "text/plain" }),
    );
    await expect(args.add).toHaveBeenCalledTimes(1);
    await expect(args.add).toHaveBeenCalledWith([
      expect.objectContaining({ name: "brief.txt", type: "text/plain" }),
    ]);
  },
};

export const RemoveAttachment: Story = {
  args: { assets: [sampleAttachment] },
  play: async ({ args, canvasElement, userEvent }) => {
    await userEvent.click(
      within(canvasElement).getByRole("button", { name: "Remove brief.txt" }),
    );
    await expect(args.remove).toHaveBeenCalledWith(sampleAttachment.id);
  },
};

export const Uploading: Story = {
  args: { uploading: true },
  play: async ({ canvasElement }) => {
    await expect(
      within(canvasElement).getByRole("button", { name: "Attach files" }),
    ).toBeDisabled();
  },
};
