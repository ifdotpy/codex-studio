import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, within, waitFor } from "storybook/test";
import { useState, type ContextType } from "react";
import { Paper, Stack } from "@mantine/core";
import { AgentSetupPicker, type SetupRole } from "../agents/AgentSetupPicker";
import { ExecutionSummaryTrigger } from "../agents/ExecutionSummaryTrigger";
import { ExecutionSettings } from "../agents/ExecutionSettings";
import { ModelCatalogReader } from "../agents/WorkerModelPicker";
import type { Agent } from "../../types";
import "../agents/chat-settings-dialog.css";
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
        modelLabel={shared ? "Main agent model" : "Default subagent model"}
        efforts={["low", "medium", "high", "xhigh", "max", "ultra"].map(
          (value) => ({ value, label: value }),
        )}
        effort={effort}
        onEffort={onEffort}
        effortLabel={shared ? "Main agent reasoning" : "Reasoning"}
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
const allRoles = async ({ canvasElement }: { canvasElement: HTMLElement }) => {
  const canvas = within(canvasElement);
  const roles = canvas.getByRole("radiogroup", {
    name: "Agent role",
  });
  for (const name of ["Lead", "Workers", "Review"])
    await expect(
      within(roles).getByRole("radio", { name }),
    ).toBeInTheDocument();
};
export const AgentSetupSheet: Story = {
  render: () => <Profile />,
  play: allRoles,
};
export const ManyModels: Story = {
  render: () => <Profile many />,
  play: allRoles,
};
export const SharedChatPicker: Story = {
  render: () => <Profile shared />,
  play: async ({ canvasElement, userEvent }) => {
    await allRoles({ canvasElement });
    const canvas = within(canvasElement);
    const model = canvas.getByRole("listbox", { name: "Main agent model" });
    await expect(model).toBeVisible();
    await expect(
      canvas.queryByText("Main agent model"),
    ).not.toBeInTheDocument();
    await expect(
      canvas.queryByText("Main agent reasoning"),
    ).not.toBeInTheDocument();
    await expect(
      canvas.queryByText("General coding and planning."),
    ).not.toBeInTheDocument();
    await expect(
      canvas.getByRole("radiogroup", { name: "Main agent reasoning" }),
    ).toBeVisible();
    const rows = within(model);
    await userEvent.click(rows.getByRole("option", { name: "Sol 6.1" }));
    await expect(rows.getByRole("option", { name: "Sol 6.1" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    await expect(
      rows.getByRole("option", { name: "Unavailable model" }),
    ).toBeDisabled();
  },
};
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

const previewCatalog = [
  {
    model: "gpt-6-astra",
    isDefault: true,
    description: "General coding and planning.",
  },
  {
    model: "gpt-6-luna",
    isDefault: false,
    description: "Fast implementation tasks.",
  },
  { model: "gpt-6.1-sol", isDefault: false, description: "Complex reasoning." },
].map((row) => ({
  ...row,
  provider: "codex",
  supportedReasoningEfforts: [
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "ultra",
  ].map((reasoningEffort) => ({ reasoningEffort })),
  serviceTiers: [{ id: "priority" }],
}));
const previewReader: ContextType<typeof ModelCatalogReader> = {
  scope: "execution-panel-story",
  read: async () => ({ data: previewCatalog }),
  subscribeState: (accept) => {
    accept({
      runtime: {
        modelsByAccount: {
          default: previewCatalog.map((model, catalogPosition) => ({
            ...model,
            accountKey: "default",
            provider: "codex",
            catalogPosition,
          })),
        },
      },
      entitiesComplete: true,
    });
    return () => {};
  },
};
const previewAgent = {
  id: "execution-preview",
  name: "Preview",
  source: "managed",
  isLead: true,
  provider: "codex",
  model: "gpt-6-astra",
  effort: "high",
  status: "idle",
  created: 1,
  workerDefaults: { model: "gpt-6-luna", effort: "high" },
  reviewDefaults: { model: null, effort: null },
} as Agent;
function InlineSettings({ worker = false }: { worker?: boolean }) {
  return (
    <ModelCatalogReader.Provider value={previewReader}>
      <Stack
        className="chat-execution-settings"
        gap="xs"
        maw="var(--studio-form-max)"
        mx="auto"
      >
        <ExecutionSettings
          presentation="panel"
          settingsRow
          initialRole="worker"
          agent={
            worker
              ? { ...previewAgent, id: "worker-preview", isLead: false }
              : previewAgent
          }
          lead={worker ? previewAgent : undefined}
          catalog={{
            models: previewCatalog,
            loading: false,
            error: "",
            retry: () => {},
          }}
          refresh={async () => {}}
        />
      </Stack>
    </ModelCatalogReader.Provider>
  );
}
export const InlineAgentSettings: Story = {
  render: () => <InlineSettings />,
  play: allRoles,
};
export const WorkerChatSettings: Story = {
  render: () => <InlineSettings worker />,
  play: async ({ canvasElement, userEvent }) => {
    await allRoles({ canvasElement });
    const canvas = within(canvasElement);
    await expect(
      canvas.getByRole("listbox", { name: "Subagent model" }),
    ).toBeVisible();
    for (const role of ["Lead", "Review"]) {
      await userEvent.click(
        within(
          canvas.getByRole("radiogroup", { name: "Agent role" }),
        ).getByText(role, { exact: true }),
      );
      await expect(
        canvas.getByText(
          "Inherited from the lead. Open the lead chat to change these settings.",
        ),
      ).toBeVisible();
      for (const model of canvas.getAllByRole("option"))
        await expect(model).toBeDisabled();
    }
    await userEvent.click(
      within(canvas.getByRole("radiogroup", { name: "Agent role" })).getByText(
        "Workers",
        { exact: true },
      ),
    );
    await expect(
      canvas.getByRole("listbox", { name: "Subagent model" }),
    ).toBeVisible();
  },
};

export const ComposerRolePopover: Story = {
  render: () => (
    <ModelCatalogReader.Provider value={previewReader}>
      <div style={{ padding: "var(--mantine-spacing-md)" }}>
        <ExecutionSettings
          agent={previewAgent}
          catalog={{
            models: previewCatalog,
            loading: false,
            error: "",
            retry: () => {},
          }}
          refresh={async () => {}}
          renderTrigger={({ agent, opened, ref, onClick }) => (
            <ExecutionSummaryTrigger
              ref={ref}
              agent={agent}
              team={[]}
              aria-expanded={opened}
              onOpen={onClick}
            />
          )}
        />
      </div>
    </ModelCatalogReader.Provider>
  ),
  play: async ({ canvasElement, userEvent }) => {
    const canvas = within(canvasElement);
    const body = within(canvasElement.ownerDocument.body);
    await userEvent.click(
      canvas.getByRole("button", { name: /^Execution settings:/ }),
    );
    await waitFor(() =>
      expect(
        body.getByRole("dialog", { name: "Main agent settings" }),
      ).toBeVisible(),
    );
    const lead = body.getByRole("radio", { name: "Lead" });
    lead.focus();
    await userEvent.keyboard("{ArrowRight}");
    await expect(body.getByRole("radio", { name: "Workers" })).toBeChecked();
    await expect(body.getByRole("radio", { name: "Workers" })).toHaveFocus();
    await userEvent.keyboard("{ArrowRight}");
    await expect(body.getByRole("radio", { name: "Review" })).toBeChecked();
    await userEvent.keyboard("{Escape}");
    await waitFor(() =>
      expect(
        canvas.getByRole("button", { name: /^Execution settings:/ }),
      ).toHaveFocus(),
    );
  },
};
