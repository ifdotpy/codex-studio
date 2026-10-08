import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, within } from "storybook/test";
import AnswerFields from "./AnswerFields";

const meta = {
  title: "Questions/Answer fields",
  component: AnswerFields,
} satisfies Meta<typeof AnswerFields>;

export default meta;
type Story = StoryObj<typeof meta>;

export const ChoicesAndFreeText: Story = {
  args: {
    questions: [
      {
        id: "approach",
        isOther: true,
        question: "Which approach should I use?",
        options: [
          { label: "Small change", description: "Keep the patch focused." },
          { label: "Full refactor" },
        ],
      },
    ],
    values: { approach: { selected: ["Small change"], other: "" } },
    onChange: fn(),
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);
    await userEvent.click(canvas.getByRole("button", { name: /Small change/ }));
    await expect(args.onChange).toHaveBeenCalledWith("approach", {
      selected: ["Small change"],
      other: "",
    });
  },
};

export const MultipleQuestions: Story = {
  args: {
    questions: [
      { id: "name", question: "What name should appear?" },
      { id: "token", question: "Enter the API token.", isSecret: true },
    ],
    values: { name: "Studio" },
    onChange: fn(),
  },
};
