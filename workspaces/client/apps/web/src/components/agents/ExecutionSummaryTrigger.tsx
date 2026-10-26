import type { Agent } from "../../types";
import type { ComponentProps } from "react";
import { ActionButton } from "../ui/primitives";
import { modelDisplayName, modelForValue } from "../models/modelOption";
import { settingsFor } from "./ExecutionSettings";
import { useWorkerModels } from "./WorkerModelPicker";

/** The composer anchor for the full role picker. */
export function ExecutionSummaryTrigger({
  agent,
  team,
  lead: teamLead,
  onOpen,
  className,
  ...buttonProps
}: ComponentProps<typeof ActionButton> & {
  agent: Agent;
  team: Agent[];
  lead?: Agent | null | undefined;
  onOpen: () => void;
}) {
  const root = agent.isLead
    ? agent
    : teamLead ||
      team.find((row) => row.id === agent.rootId && row.isLead) ||
      agent;
  const main = settingsFor(agent, false);
  const defaults = settingsFor(root, true);
  const catalog = useWorkerModels(agent.accountKey || "default", true);
  const workers = useWorkerModels(
    defaults.account_key || root.accountKey || "default",
    true,
    !defaults.account_key,
  );
  const summary = (
    model: string | null | undefined,
    effort: string | null | undefined,
    worker = false,
    review = false,
  ) => {
    const rows = review
      ? workers.models.filter((row) => row.provider !== "claude")
      : worker
        ? workers.models
        : catalog.models;
    const info = modelForValue(rows, model || "default");
    const display = modelForValue(rows, info?.resolvedModel || model) || info;
    return {
      name: modelDisplayName(display, model || "Default"),
      provider: review
        ? "codex"
        : info?.provider ||
          (/^claude-|^(opus|sonnet|haiku)/.test(model || "")
            ? "claude"
            : !worker
              ? agent.provider || "codex"
              : "codex"),
      effort: (effort || info?.defaultReasoningEffort || "").replace(
        /^./,
        (letter) => letter.toUpperCase(),
      ),
    };
  };
  const lead = summary(main.model, main.effort);
  const worker = summary(
    defaults.model ?? main.model,
    defaults.effort ?? main.effort,
    true,
  );
  const review = root.reviewDefaults?.model
    ? summary(
        root.reviewDefaults.model,
        root.reviewDefaults.effort ?? main.effort,
        true,
        true,
      )
    : null;
  const entries = [
    {
      role: "lead",
      label: agent.isLead ? "Lead" : "Agent",
      value: lead,
    },
    ...(root.isLead
      ? [
          { role: "worker", label: "Workers", value: worker },
          { role: "review", label: "Review", value: review },
        ]
      : []),
  ];
  const full =
    "Execution settings: " +
    entries
      .map(({ label, value }) =>
        value
          ? `${label} ${value.name} ${value.effort} (${value.provider})`.trim()
          : `${label} same as caller`,
      )
      .join("; ");
  return (
    <ActionButton
      className={["execution-menu execution-summary-trigger", className]
        .filter(Boolean)
        .join(" ")}
      aria-label={full}
      title={full}
      aria-haspopup="dialog"
      onClick={onOpen}
      {...buttonProps}
    >
      <span className="execution-summary-segments" aria-hidden="true">
        {entries.map(({ role, label, value }) => (
          <span
            className="execution-summary-segment"
            data-role={role}
            key={role}
          >
            <span className="execution-role">{label}</span>
            {value ? (
              <span className="execution-model">{value.name}</span>
            ) : (
              <span className="execution-inherited-label">Same as caller</span>
            )}
          </span>
        ))}
      </span>
    </ActionButton>
  );
}
