import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, within } from "storybook/test";
import { LinuxVMSettings } from "./LinuxVMSettings";

const GiB = 1024 ** 3;
const meta = {
  title: "Settings/Linux VM",
  component: LinuxVMSettings,
  args: { active: true },
  beforeEach: () => {
    const original = window.fetch;
    window.fetch = async (input, init) => {
      const request = new Request(input, init);
      if (new URL(request.url).pathname === "/api/linux-vm/settings")
        return new Response(
          JSON.stringify({
            settings:
              request.method === "POST"
                ? JSON.parse(await request.text())
                : {
                    cpus: 4,
                    memoryBytes: 4 * GiB,
                    systemDiskBytes: 16 * GiB,
                    dataDiskBytes: 128 * GiB,
                  },
            state: "running",
            allocatedDiskBytes: 1024,
          }),
          { headers: { "content-type": "application/json" } },
        );
      return original(input, init);
    };
    return () => {
      window.fetch = original;
    };
  },
} satisfies Meta<typeof LinuxVMSettings>;
export default meta;
type Story = StoryObj<typeof meta>;

export const ActiveWorkersKeepTheirLimits: Story = {
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    await expect(await canvas.findByRole("status")).toHaveTextContent(
      "VM state: running",
    );
    await expect(
      canvas.getByRole("button", { name: "Save VM limits" }),
    ).toBeDisabled();
    await expect(
      canvas.getByRole("textbox", { name: "Processor cores" }),
    ).toBeDisabled();
  },
};
