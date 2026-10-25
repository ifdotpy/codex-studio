import { TeamRetainedWorktrees } from "./RetainedWorktrees";
import { ActionIcon, Button, TextInput } from "@mantine/core";
import { ArrowLeft, Search, X } from "lucide-react";
import type { Agent } from "../../types";
import { useMemo, type ComponentProps } from "react";
import { useSubscription } from "../../sync/subscriptions";
import type { Account } from "../Accounts";
import type { ChatIndicator } from "../chat-status/chatStatusModel";
import type { PreviewTarget } from "../FilePreview";
import UIErrorBoundary from "../UIErrorBoundary";
import WorkerCard from "./WorkerCard";
import Disclosure from "../ui/Disclosure";
import {
  TeamModels,
  TeamSummary,
  workerState,
  teamDispatchGroups,
  workerDepth,
  orderWorkerHierarchy,
  TeamGroupLabel,
  filterTeamWorkers,
} from "./WorkerOverview";

type Props = {
  lead?: Agent | null;
  workers: Agent[];
  retainedWorktrees?: Parameters<typeof TeamRetainedWorktrees>[0]["items"];
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

/** Only mounted roster rows retain detailed progress and result subscriptions. */
function LiveWorkerCard(props: ComponentProps<typeof WorkerCard>) {
  const detail = useSubscription({
    kind: "agentDetail",
    agentId: props.agent.id,
  });
  const agent = useMemo(
    () => (detail.data ? { ...detail.data, ...props.agent } : props.agent),
    [detail.data, props.agent],
  );
  return <WorkerCard {...props} agent={agent} />;
}

export default function TeamPanel({
  lead,
  workers,
  retainedWorktrees = [],
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
  // Keep the roster, counters and row indicator aligned with the worker's
  // operational approval state, even before its request projection arrives.
  const teamIndicators = new Map(indicators);
  for (const agent of workers) {
    if (
      workerState(agent, answerIds, deferredIds) === "answer" &&
      teamIndicators.get(agent.id)?.kind !== "answer"
    )
      teamIndicators.set(agent.id, {
        kind: "answer",
        label: "Needs your answer",
      });
  }
  const byId = new Map(
    [...(lead ? [lead] : []), ...workers].map((a) => [a.id, a]),
  );
  const worker = (a: Agent) => (
    <UIErrorBoundary key={a.id} label="this subagent" resetKey={a.id}>
      <LiveWorkerCard
        agent={a}
        depth={workerDepth(a, byId, lead?.id)}
        accounts={accounts}
        selected={opened === a.id}
        awaitingAnswer={workerState(a, answerIds, deferredIds) === "answer"}
        deferred={deferredIds.has(a.id)}
        indicator={teamIndicators.get(a.id)}
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
  const query = workerQuery.trim().toLowerCase();
  const ordered = orderWorkerHierarchy(workers);
  const shown = query
    ? ordered.filter((a) =>
        `${a.name} ${a.status} ${a.role || ""} ${a.overview?.task || ""} ${a.overview?.result || ""}`
          .toLowerCase()
          .includes(query),
      )
    : filterTeamWorkers(ordered, teamIndicators, workerFilter);
  const { groups, completed } = teamDispatchGroups(shown, teamIndicators);
  const smallTeam = workers.length <= 3;
  const showTeamFilters = !smallTeam || !!workerQuery || workerFilter !== "all";
  return (
    <TeamModels accountKey={lead?.accountKey || "default"}>
      <aside
        id="team"
        aria-label="Team"
        className={`team-dispatch ${smallTeam ? "team-compact" : ""}`}
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
        {lead && (lead.overview?.result || lead.overview?.resultFile) && (
          <Disclosure
            className="team-lead-report"
            summary="Main agent report"
            chevron
          >
            {lead.overview.result && <p>{lead.overview.result}</p>}
            {lead.overview.resultTruncated && (
              <Button
                variant="subtle"
                size="compact-xs"
                onClick={() => open(lead.id)}
              >
                Continue in main chat
              </Button>
            )}
            {lead.overview.resultFile && (
              <Button
                component="a"
                href={lead.overview.resultFile}
                variant="subtle"
                size="compact-xs"
                onClick={(event) => {
                  event.preventDefault();
                  previewResult({
                    agent: lead.id,
                    path: lead.overview?.resultFile ?? undefined,
                  });
                }}
              >
                Open main agent submitted result
              </Button>
            )}
          </Disclosure>
        )}
        <TeamRetainedWorktrees items={retainedWorktrees} />
        <TeamSummary
          workers={workers}
          indicators={teamIndicators}
          selected={workerFilter}
          onFilter={(value) => {
            setWorkerFilter(value);
            if (value === "completed") setCompletedOpen(true);
          }}
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
            {!!query && (
              <p className="team-search-count" role="status">
                {shown.length} {shown.length === 1 ? "match" : "matches"} in
                this team
              </p>
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
                <h3>
                  <TeamGroupLabel
                    kind={group.kind}
                    label={group.name}
                    count={group.workers.length}
                  />
                </h3>
                {group.workers.map(worker)}
              </section>
            ) : null,
          )}
          {!!completed.length && (
            <Disclosure
              className="worker-group"
              open={!!query || completedOpen}
              onChange={(open) => {
                if (!query) setCompletedOpen(open);
              }}
              ariaLabel={`Finished (${completed.length})`}
              chevron
              summary={
                <TeamGroupLabel
                  kind="completed"
                  label="Finished"
                  count={completed.length}
                />
              }
            >
              {/* A collapsed group keeps its rows out of the DOM; the control
                  and the team overview still count them. */}
              {(!!query || completedOpen) && completed.map(worker)}
            </Disclosure>
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
