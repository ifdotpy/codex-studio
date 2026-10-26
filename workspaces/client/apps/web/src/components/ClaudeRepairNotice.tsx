import { useEffect, useState } from "react";
import { useServerApi } from "../servers/ServerServicesContext";
import type { Agent } from "../types";
import {
  missingRepairEvidence,
  repairReason,
  type ClaudeRepairReport,
} from "./claudeRepairModel";
import "./claude-repair-notice.css";

/** Read saved evidence only. This component never starts a session repair or a turn. */
export default function ClaudeRepairNotice({ agent }: { agent: Agent }) {
  const { get } = useServerApi();
  const [result, setResult] = useState<{
    scope: string;
    report?: ClaudeRepairReport;
    error?: string;
  }>();
  const scope = JSON.stringify([
    agent.id,
    agent.threadId,
    agent.epoch,
    agent.accountKey,
  ]);
  const [refreshVersion, setRefreshVersion] = useState(0);
  // A new scope hides old evidence below; reading it requires explicit refresh.
  useEffect(() => {
    let active = true;
    if (!agent.threadId) return;
    get("/api/claude/repair-report", { query: { id: agent.id } })
      .then((report) => {
        if (active) setResult({ scope, report });
      })
      .catch(() => {
        if (active)
          setResult({
            scope,
            error: "The saved repair report is unavailable.",
          });
      });
    return () => {
      active = false;
    };
  }, [get, refreshVersion, agent.id]);
  const current = result?.scope === scope ? result : undefined;
  const repairRequired =
    /CLAUDE_SESSION_REPAIR_REQUIRED|needs repair|journal.*repair/i.test(
      JSON.stringify(agent.error || ""),
    );
  const report = current?.report;
  if ((!report || report.status === "none") && !repairRequired) return null;
  const missing = report ? missingRepairEvidence(report) : [];
  return (
    <aside
      className="claude-repair-notice"
      role="status"
      aria-label="Claude journal repair"
    >
      <strong>
        {report?.status === "repaired"
          ? "Journal repaired"
          : "Journal needs repair"}
      </strong>
      <p>
        {report && report.status !== "none"
          ? repairReason(report)
          : "The bridge cannot confirm the saved session."}
      </p>
      <p>
        Skipped rows:{" "}
        {report && report.status !== "none" ? report.skippedRows : "unknown"}.
      </p>
      <details>
        <summary>Repair report</summary>
        <button
          type="button"
          onClick={() => setRefreshVersion((value) => value + 1)}
        >
          Refresh saved report
        </button>
        {report && report.status !== "none" ? (
          <>
            <p>
              This report contains saved evidence. It does not change the
              session.
            </p>
            <dl>
              <dt>Journal sequence</dt>
              <dd>
                {report.invalidSequence
                  ? "Invalid or incomplete"
                  : "No sequence gap reported"}
              </dd>
              <dt>Session and workspace</dt>
              <dd>
                {report.scopeConfirmed === true ? "Confirmed" : "Unconfirmed"}
              </dd>
              <dt>Native history</dt>
              <dd>
                {report.nativeHistoryConfirmed === true
                  ? "Confirmed"
                  : "Unconfirmed"}
              </dd>
            </dl>
            <strong>Missing evidence</strong>
            {missing.length ? (
              <ul>
                {missing.map((item) => (
                  <li key={item}>{item}</li>
                ))}
              </ul>
            ) : (
              <p>No missing evidence was reported.</p>
            )}
          </>
        ) : (
          <p>{current?.error || "No saved report is available."}</p>
        )}
        <p>No repair or replay action is available here.</p>
      </details>
    </aside>
  );
}
