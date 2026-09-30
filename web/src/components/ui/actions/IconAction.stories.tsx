import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, within } from "storybook/test";
import { Plus, X } from "lucide-react";
import { IconAction } from "./IconAction";

const meta = {
  title: "UI/Actions/IconAction",
  component: IconAction,
  args: {
    label: "Add item",
    children: <Plus size={16} />,
    onClick: fn(),
  },
  argTypes: {
    children: { control: false },
  },
} satisfies Meta<typeof IconAction>;

export default meta;
type Story = StoryObj<typeof meta>;

export const Default: Story = {
  play: async ({ args, canvasElement, userEvent }) => {
    const action = within(canvasElement).getByRole("button", {
      name: "Add item",
    });
    await userEvent.click(action);
    await expect(args.onClick).toHaveBeenCalledTimes(1);
  },
};

function ToggleAction() {
  const [active, setActive] = useState(false);
  return (
    <IconAction
      label={active ? "Remove favorite" : "Add favorite"}
      variant={active ? "filled" : "default"}
      color={active ? "red" : undefined}
      onClick={() => setActive((current) => !current)}
    >
      {active ? <X size={16} /> : <Plus size={16} />}
    </IconAction>
  );
}

export const Toggle: Story = {
  render: () => <ToggleAction />,
  play: async ({ canvasElement, userEvent }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByRole("button", { name: "Add favorite" }));
    await expect(
      canvas.getByRole("button", { name: "Remove favorite" }),
    ).toBeVisible();
  },
};

export const Disabled: Story = {
  args: { disabled: true },
  play: async ({ canvasElement }) => {
    await expect(
      within(canvasElement).getByRole("button", { name: "Add item" }),
    ).toBeDisabled();
  },
};
