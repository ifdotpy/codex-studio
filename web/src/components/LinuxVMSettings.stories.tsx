import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, userEvent, within } from "storybook/test";
import { LinuxVMSettings } from "./LinuxVMSettings";

const GiB = 1024 ** 3;
let savedSettings: unknown;
const meta = {
  title: "Settings/Linux VM",
  component: LinuxVMSettings,
  args: { active: true },
  beforeEach: ({ parameters }) => {
    savedSettings = undefined;
    const original = window.fetch;
    window.fetch = async (input, init) => {
      const request = new Request(input, init);
      if (new URL(request.url).pathname === "/api/linux-vm/settings") {
        if (request.method === "POST")
          savedSettings = JSON.parse(await request.text());
        return new Response(
          JSON.stringify({
            settings:
              request.method === "POST"
                ? savedSettings
                : {
                    cpus: 4,
                    memoryBytes: 4 * GiB,
                    systemDiskBytes: 16 * GiB,
                    dataDiskBytes: 128 * GiB,
                  },
            state: parameters.vmState ?? "running",
            allocatedDiskBytes: 1024,
          }),
          { headers: { "content-type": "application/json" } },
        );
      }
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

export const SaveStoppedLimits: Story = {
  parameters: { vmState: "stopped" },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);
    const cores = await canvas.findByRole("textbox", {
      name: "Processor cores",
    });
    await userEvent.clear(cores);
    await userEvent.type(cores, "6");
    await userEvent.click(
      canvas.getByRole("button", { name: "Save VM limits" }),
    );
    await expect(savedSettings).toMatchObject({
      cpus: 6,
      memoryBytes: 4 * GiB,
    });
  },
};
