import { ActionIcon, Button, TextInput, UnstyledButton } from "@mantine/core";
import { ArrowLeft, Search, X } from "lucide-react";
import type { Agent } from "../../types";
import { busy } from "../../types";
import type { Account } from "../Accounts";
import type { ChatIndicator } from "../chat-status/chatStatusModel";
import type { PreviewTarget } from "../FilePreview";
import UIErrorBoundary from "../UIErrorBoundary";
import WorkerCard from "./WorkerCard";
import {
  TeamModels,
  TeamSummary,
  workerState,
  TEAM_PANEL_STATES,
} from "./WorkerOverview";

type Props = {
  lead?: Agent | null;
  workers: Agent[];
  accounts: Account[];
  opened: string | null;
  answerIds: Set<string>;
  deferredIds: Set<string>;
  indicators: Map<string, ChatIndicator>;
  workerQuery: string;
  setWorkerQuery: (query: string) => void;
  workerFilter: string;
  setWorkerFilter: (filter: string) => void;
  completedOpen: boolean;
  setCompletedOpen: (open: boolean) => void;
  setTeamOpen: (open: boolean) => void;
  setWideTeamOpen: (open: boolean) => void;
  saveClosed: () => void;
  open: (id: string) => void;
  previewResult: (target: PreviewTarget) => void;
  remove: (id: string) => void;
};

export default function TeamPanel({
  lead,
  workers,
  accounts,
  opened,
  answerIds,
  deferredIds,
  indicators,
  workerQuery,
  setWorkerQuery,
  workerFilter,
  setWorkerFilter,
  completedOpen,
  setCompletedOpen,
  setTeamOpen,
  setWideTeamOpen,
  saveClosed,
  open,
  previewResult,
  remove,
}: Props) {
  const worker = (a: Agent) => (
    <UIErrorBoundary key={a.id} label="this subagent" resetKey={a.id}>
      <WorkerCard
        agent={a}
        accounts={accounts}
        selected={opened === a.id}
        awaitingAnswer={workerState(a, answerIds, deferredIds) === "answer"}
        deferred={deferredIds.has(a.id)}
        indicator={indicators.get(a.id)}
        open={() => open(a.id)}
        previewResult={() =>
          previewResult({
            agent: a.id,
            path: a.overview?.resultFile ?? undefined,
          })
        }
        remove={() => {
          setTeamOpen(false);
          remove(a.id);
        }}
      />
    </UIErrorBoundary>
  );
  // Failed workers are the lead's work; the filter only finds them.
  const failed = (a: Agent) =>
    workerState(a, answerIds, deferredIds) === "attention";
  const query = workerQuery.trim().toLowerCase();
  const shown = workers.filter((a) =>
    query
      ? `${a.name} ${a.status} ${a.role || ""} ${a.overview?.task || ""} ${a.overview?.result || ""}`
          .toLowerCase()
          .includes(query)
      : workerFilter === "attention"
        ? failed(a)
        : workerFilter === "active"
          ? busy.has(a.status ?? "")
          : true,
  );
  // The same states and names as the team summary; Finished stays collapsed below.
  const panelState = (a: Agent) =>
    a.inFlight && ["running", "starting"].includes(a.status ?? "")
      ? "working"
      : workerState(a, answerIds, deferredIds);
  const groups = TEAM_PANEL_STATES.filter(
    ([state]) => state !== "completed",
  ).map(([state, name]) => ({
    name,
    workers: shown.filter((a) => panelState(a) === state),
  }));
  const completed = shown.filter(
    (a) => workerState(a, answerIds, deferredIds) === "completed",
  );
  const smallTeam = workers.length <= 3;
  const showTeamFilters = !smallTeam || !!workerQuery || workerFilter !== "all";
  return (
    <TeamModels accountKey={lead?.accountKey || "default"}>
      <aside
        id="team"
        aria-label="Team"
        className={smallTeam ? "team-compact" : undefined}
      >
        <div className="team-heading">
          <h2>Team</h2>
          <ActionIcon
            aria-label="Close team"
            id="team-close"
            onClick={() => {
              setTeamOpen(false);
              setWideTeamOpen(false);
              saveClosed();
            }}
          >
            <X size={16} />
          </ActionIcon>
        </div>
        <TeamSummary
          workers={workers}
          answers={answerIds}
          deferred={deferredIds}
        />
        {/* The lead link is only useful from a worker chat. */}
        {lead && opened !== lead.id && (
          <Button
            id="lead-row"
            variant="subtle"
            size="compact-sm"
            title={lead.name || "Main agent"}
            leftSection={<ArrowLeft size={13} />}
            onClick={() => open(lead.id)}
          >
            Back to main agent
          </Button>
        )}
        {showTeamFilters && (
          <>
            <TextInput
              leftSection={<Search size={14} />}
              id="worker-search"
              type="search"
              aria-label="Find a subagent"
              placeholder="Find a worker"
              value={workerQuery}
              onChange={(e) => setWorkerQuery(e.target.value)}
            />
            {query ? (
              <p className="team-search-count" role="status">
                {shown.length} {shown.length === 1 ? "match" : "matches"} in
                this team
              </p>
            ) : (
              <div
                className="team-filters"
                role="group"
                aria-label="Filter subagents"
              >
                {[
                  ["all", "All"],
                  ["active", "Active"],
                  ["attention", "Failed"],
                ].map(([value, label]) => (
                  <UnstyledButton
                    key={value}
                    aria-pressed={workerFilter === value}
                    onClick={() => setWorkerFilter(String(value))}
                  >
                    {label}
                  </UnstyledButton>
                ))}
              </div>
            )}
          </>
        )}
        <div id="workers">
          {groups.map((group) =>
            group.workers.length ? (
              <section
                className="team-status-group"
                aria-label={group.name}
                key={group.name}
              >
                {!smallTeam && <h3>{group.name}</h3>}
                {group.workers.map(worker)}
              </section>
            ) : null,
          )}
          {!!completed.length && (
            <details
              className="worker-group"
              open={!!query || completedOpen}
              onToggle={(event) => {
                if (!query) setCompletedOpen(event.currentTarget.open);
              }}
            >
              <summary>Finished</summary>
              {/* A collapsed group keeps its rows out of the DOM; the summary
                  and the team overview still count them. */}
              {(!!query || completedOpen) && completed.map(worker)}
            </details>
          )}
          {!shown.length && (
            <p className="team-empty" role="status">
              {query
                ? "No workers match your search."
                : "No workers in this group."}
            </p>
          )}
        </div>
      </aside>
    </TeamModels>
  );
}
