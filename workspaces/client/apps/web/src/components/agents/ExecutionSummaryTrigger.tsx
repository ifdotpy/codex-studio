import { Bot, Equal, ScanEye, UserRound } from "lucide-react";
import type { Agent } from "../../types";
import { ProviderMark } from "../AccountTiles";
import { ActionButton } from "../ui/primitives";
import { modelDisplayName, modelForValue } from "../models/modelOption";
import { settingsFor } from "./ExecutionSettings";
import { useWorkerModels } from "./WorkerModelPicker";

/** One composer entry point; the dialog supplies full-size role targets. */
export function ExecutionSummaryTrigger({
  agent,
  team,
  onOpen,
}: {
  agent: Agent;
  team: Agent[];
  onOpen: () => void;
}) {
  const root = agent.isLead
    ? agent
    : team.find((row) => row.id === agent.rootId && row.isLead) || agent;
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
  const count = team.filter(
    (row) => row.rootId === root.id && row.id !== root.id && !row.deletedAt,
  ).length;
  const entries = [
    {
      role: "lead",
      label: agent.isLead ? "Lead" : "Agent",
      Icon: UserRound,
      value: lead,
    },
    ...(root.isLead
      ? [
          { role: "worker", label: "Workers", Icon: Bot, value: worker },
          { role: "review", label: "Review", Icon: ScanEye, value: review },
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
      className="execution-menu execution-summary-trigger"
      aria-label={full}
      title={full}
      aria-haspopup="dialog"
      onClick={onOpen}
    >
      <span className="execution-summary-segments" aria-hidden="true">
        {entries.map(({ role, label, Icon, value }) => (
          <span
            className="execution-summary-segment"
            data-role={role}
            key={role}
          >
            <span className="execution-symbols">
              <Icon className="execution-role-icon" />
              {value ? (
                <span
                  className="execution-provider"
                  data-provider={value.provider}
                >
                  <ProviderMark provider={value.provider} />
                </span>
              ) : (
                <Equal className="execution-inherited-icon" />
              )}
            </span>
            <span className="execution-role">{label}</span>
            {value ? (
              <>
                <span className="execution-model">{value.name}</span>
                <span className="execution-effort">{value.effort}</span>
              </>
            ) : (
              <>
                <span className="execution-inherited-label">
                  Same as caller
                </span>
              </>
            )}
            {role === "worker" && (
              <small className="execution-worker-count">{count}</small>
            )}
          </span>
        ))}
      </span>
    </ActionButton>
  );
}
