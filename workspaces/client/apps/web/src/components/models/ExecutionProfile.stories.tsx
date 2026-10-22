import type { Meta, StoryObj } from "@storybook/react-vite";
import { useState } from "react";
import { Paper, Stack } from "@mantine/core";
import { AgentSetupPicker, type SetupRole } from "../agents/AgentSetupPicker";
import { ModelField } from "./ModelField";
import {
  INHERITED_MODEL,
  toModelOptions,
  type ModelOption,
} from "./modelOption";
import "../agents/execution-settings.css";

const catalog = toModelOptions([
  {
    model: "gpt-6-astra",
    description: "General coding and planning.",
    isDefault: true,
  },
  { model: "gpt-6.1-sol", description: "Complex coding tasks." },
  {
    model: "claude-opus-4-6",
    provider: "claude",
    description: "Claude Opus 4.6 · Careful reasoning.",
  },
]);
const inherited: ModelOption = {
  value: INHERITED_MODEL,
  label: "Same as main agent",
  provider: "codex",
  description: "Inherit the main agent model.",
  badges: [],
};
const disabled: ModelOption = {
  value: "unavailable",
  label: "Unavailable model",
  provider: "codex",
  badges: [],
  disabledReason: "Unavailable in standard mode.",
};
const options = [inherited, ...catalog, disabled];

const meta = {
  title: "Models/Execution profile",
  parameters: { layout: "fullscreen" },
} satisfies Meta;
export default meta;
type Story = StoryObj<typeof meta>;

function Profile({
  shared = false,
  many = false,
}: {
  shared?: boolean;
  many?: boolean;
}) {
  const [role, onRole] = useState<SetupRole>(
    shared ? "orchestrator" : "worker",
  );
  const [model, onModel] = useState(shared ? "gpt-6-astra" : INHERITED_MODEL);
  const [effort, onEffort] = useState("high");
  const [fast, setFast] = useState(false);
  const [daybreak, setDaybreak] = useState(false);
  return (
    <Paper
      withBorder
      p="sm"
      radius="md"
      maw="var(--mantine-breakpoint-xs)"
      mx="auto"
    >
      <AgentSetupPicker
        role={role}
        onRole={onRole}
        roles={shared ? [] : ["orchestrator", "worker", "review"]}
        provider="codex"
        providers={[]}
        onProvider={() => {}}
        accounts={[]}
        accountKey=""
        onAccount={() => {}}
        models={
          many
            ? [
                ...options,
                ...toModelOptions(
                  Array.from({ length: 16 }, (_, index) => ({
                    model: `custom-${index + 1}`,
                    displayName: `Additional model ${index + 1}`,
                  })),
                ),
              ]
            : shared
              ? [...catalog, disabled]
              : options
        }
        model={model}
        onModel={onModel}
        modelLabel={shared ? "Model for agent 1" : "Default subagent model"}
        efforts={["low", "medium", "high", "xhigh", "max", "ultra"].map(
          (value) => ({ value, label: value }),
        )}
        effort={effort}
        onEffort={onEffort}
        effortLabel="Reasoning"
        disabled={false}
        fastAvailable={!shared}
        fast={fast}
        onFast={() => setFast(!fast)}
        daybreakAvailable={!shared}
        daybreak={daybreak}
        onDaybreak={() => setDaybreak(!daybreak)}
      />
    </Paper>
  );
}
export const AgentSetupSheet: Story = { render: () => <Profile /> };
export const ManyModels: Story = { render: () => <Profile many /> };
export const SharedChatPicker: Story = { render: () => <Profile shared /> };
export const WorkspaceCompact: Story = {
  render: function Compact() {
    const [value, onChange] = useState("gpt-6-astra");
    return (
      <Stack gap="sm" maw="var(--mantine-breakpoint-xs)" mx="auto">
        <ModelField
          presentation="select"
          label="Model"
          value={value}
          onChange={onChange}
          options={options.map((option) => ({
            ...option,
            badges: [
              ...option.badges,
              ...(option.value === value ? ["current"] : []),
            ],
          }))}
        />
      </Stack>
    );
  },
};
