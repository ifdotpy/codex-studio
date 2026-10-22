import { RetainedWorktree } from "./AgentLimits";
import { useEffect, useState } from "react";
import { Button, TextInput } from "@mantine/core";
import { errorText, type GetResult } from "../../api";
import type { Agent } from "../../types";
import { useServerApi } from "../../servers/ServerServicesContext";
import { MAX_SAFE_INTEGER } from "../../generated/sharedValues";
import "./agent-limits.css";

type Summary = GetResult<"/api/team-budget">;
export function budgetInput(value: string): number | null {
  if (!value.trim()) return null;
  const amount = Number(value);
  if (!Number.isSafeInteger(amount) || amount <= 0 || amount > MAX_SAFE_INTEGER)
    throw new Error(
      "Enter a positive whole number of tokens, or leave the field empty.",
    );
  return amount;
}

export function TeamBudgetView({
  summary,
  blocked,
}: {
  summary: Summary;
  blocked?: string | null;
}) {
  const spent = summary.tokensUsed;
  const incomplete = summary.incomplete.length > 0;
  return (
    <>
      <p>
        Spent: {spent == null ? "Unavailable" : spent.toLocaleString()} tokens
        {incomplete ? " (known usage)" : ""}
      </p>
      <p>
        Remaining:{" "}
        {incomplete || spent == null
          ? "Unavailable"
          : summary.tokenBudget == null
            ? "Unlimited"
            : `${Math.max(0, summary.tokenBudget - spent).toLocaleString()} tokens`}
      </p>
      {incomplete && (
        <p role="status">
          <strong>Usage accounting incomplete.</strong> New work cannot start
          until usage is verified.
        </p>
      )}
      {summary.incomplete.map((entry) => (
        <p key={entry.agentId}>
          {entry.agentId}: {entry.reason}
        </p>
      ))}
      {summary.reached && (
        <p role="status">
          Team token budget reached. Increase the budget before resuming.
        </p>
      )}
      {blocked && <p role="status">Admission blocked: {blocked}</p>}
    </>
  );
}

export default function TeamBudget({
  lead,
  members,
}: {
  lead: Agent;
  members: Agent[];
}) {
  const { get, post } = useServerApi();
  const [summary, setSummary] = useState<Summary | null>(null);
  const [input, setInput] = useState(String(lead.tokenBudget ?? ""));
  const [error, setError] = useState("");
  const [readError, setReadError] = useState("");
  const [busy, setBusy] = useState(false);
  const [revision, setRevision] = useState(0);
  const blocked = [lead, ...members].find(
    (member) => member.budgetBlocked,
  )?.budgetBlocked;
  // Re-read the server total when usage changes. The server includes archived
  // and deleted workers, which are absent from the visible team panel.
  const usageRevision = JSON.stringify(
    [lead, ...members].map((a) => [
      a.id,
      a.tokensUsed,
      a.tokenUsageAccounting,
      a.budgetBlocked,
    ]),
  );
  useEffect(
    () => setInput(String(lead.tokenBudget ?? "")),
    [lead.id, lead.tokenBudget],
  );
  useEffect(() => {
    let current = true;
    get("/api/team-budget", { query: { agent: lead.id } })
      .then((value) => {
        if (current) {
          setSummary(value);
          setReadError("");
        }
      })
      .catch((failure) => {
        if (current) {
          setSummary(null);
          setReadError(errorText(failure));
        }
      });
    return () => {
      current = false;
    };
  }, [lead.id, lead.tokenBudget, usageRevision, revision, get]);
  return (
    <>
      <section className="agent-limits" aria-label="Team token budget">
        <strong>Team token budget</strong>
        {summary ? (
          <TeamBudgetView summary={summary} blocked={blocked} />
        ) : (
          <p>Budget totals unavailable.</p>
        )}
        <TextInput
          label="Token budget"
          description="Leave empty for no limit. Includes all team members."
          value={input}
          inputMode="numeric"
          onChange={(event) => setInput(event.currentTarget.value)}
        />
        <div className="agent-limits-actions">
          <Button
            size="compact-sm"
            loading={busy}
            onClick={async () => {
              setError("");
              setBusy(true);
              try {
                const tokenBudget = budgetInput(input);
                await post("/api/configure", { id: lead.id, tokenBudget });
                setRevision((value) => value + 1);
              } catch (failure) {
                setError(errorText(failure));
              } finally {
                setBusy(false);
              }
            }}
          >
            Save budget
          </Button>
          <Button
            size="compact-sm"
            variant="subtle"
            onClick={() => setRevision((value) => value + 1)}
          >
            Refresh totals
          </Button>
        </div>
        {readError && <p role="alert">{readError}</p>}
        {error && <p role="alert">{error}</p>}
      </section>
      {summary?.retainedWorktrees
        ?.filter((item) => !members.some((member) => member.id === item.id))
        .map((item) => (
          <div key={item.id}>
            <p>{item.name || item.id}</p>
            <RetainedWorktree
              agent={{
                id: item.id,
                name: item.name,
                worktree: true,
                workspaceCleanupIntent: item.cleanup,
              }}
            />
          </div>
        ))}
    </>
  );
}
