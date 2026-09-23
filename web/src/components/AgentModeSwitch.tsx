import { useEffect, useRef, useState } from "react";
import { api, ApiError, errorText } from "../api";
import type { Agent } from "../types";
import "./agent-mode-switch.css";

type Mode = "multi" | "single";
type Confirmed = { mode: Mode; revision: number };
type Request = {
  id: string;
  agent_mode: Mode;
  expected_mode_revision: number;
  request_id: string;
};
type Stored = { confirmed?: Confirmed; pending?: Request };
type Props = {
  lead: Agent;
  stateDir: string;
  workspaceId: string;
  refresh: () => Promise<void>;
};
const validMode = (value: unknown): value is Mode =>
  value === "multi" || value === "single";
const validRevision = (value: unknown): value is number =>
  Number.isSafeInteger(value) && Number(value) >= 0;
const stateOf = (agent: Agent): Confirmed => ({
  mode: validMode(agent.agentMode) ? agent.agentMode : "multi",
  revision: validRevision(agent.agentModeRevision)
    ? agent.agentModeRevision
    : 0,
});

export default function AgentModeSwitch(props: Props) {
  const scope = JSON.stringify([
    props.stateDir,
    props.workspaceId,
    props.lead.id,
  ]);
  return (
    <ScopedAgentModeSwitch
      key={scope}
      storageKey={`studio-agent-mode:${scope}`}
      {...props}
    />
  );
}

function ScopedAgentModeSwitch({
  lead,
  workspaceId,
  storageKey,
  refresh,
}: Props & { storageKey: string }) {
  const [initial, setInitial] = useState(() => {
    try {
      const value: Stored = JSON.parse(
        localStorage.getItem(storageKey) || "{}",
      );
      if (
        !value ||
        typeof value !== "object" ||
        (value.confirmed &&
          (!validMode(value.confirmed.mode) ||
            !validRevision(value.confirmed.revision))) ||
        (value.pending &&
          (value.pending.id !== lead.id ||
            !validMode(value.pending.agent_mode) ||
            !validRevision(value.pending.expected_mode_revision) ||
            typeof value.pending.request_id !== "string" ||
            !value.pending.request_id))
      )
        throw new Error("The saved mode request cannot be read.");
      return { value, error: "" };
    } catch (cause) {
      return { value: {} as Stored, error: errorText(cause) };
    }
  });
  const [stored, setStored] = useState(initial.value);
  const [error, setError] = useState(initial.error);
  const [saving, setSaving] = useState(false);
  const lock = useRef(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const snapshot = stateOf(lead);
  const confirmed =
    stored.confirmed && stored.confirmed.revision > snapshot.revision
      ? stored.confirmed
      : snapshot;
  const latest = useRef(confirmed);
  latest.current = confirmed;
  // Keep the accepted revision through reloads while replication catches up.
  useEffect(() => {
    if (
      initial.error ||
      (stored.confirmed?.revision ?? -1) >= snapshot.revision
    )
      return;
    const next = { ...stored, confirmed: snapshot };
    try {
      localStorage.setItem(storageKey, JSON.stringify(next));
      setStored(next);
    } catch {
      // A mode command still requires a successful durable write below.
    }
  }, [snapshot.mode, snapshot.revision, stored, storageKey, initial.error]);

  const submit = async () => {
    if (lock.current || initial.error || !workspaceId) return;
    const request = stored.pending || {
      id: lead.id,
      agent_mode: confirmed.mode === "multi" ? "single" : "multi",
      expected_mode_revision: confirmed.revision,
      request_id: crypto.randomUUID(),
    };
    const pending = { confirmed, pending: request };
    // Do not send if the browser cannot retain the identity for an exact retry.
    try {
      localStorage.setItem(storageKey, JSON.stringify(pending));
    } catch {
      setError(
        "Cannot save the mode request on this device. Free browser storage and retry.",
      );
      return;
    }
    setStored(pending);
    lock.current = true;
    setSaving(true);
    setError("");
    try {
      const result = await api<Agent>("/api/conversation", request, {
        workspaceId,
        timeoutMs: 15000,
      });
      if (
        result.id !== lead.id ||
        !validMode(result.agentMode) ||
        !validRevision(result.agentModeRevision) ||
        result.agentModeRevision <= request.expected_mode_revision
      )
        throw new Error(
          "The server did not confirm the mode request. Retry the same request.",
        );
      const accepted = stateOf(result);
      const disk: Stored = JSON.parse(localStorage.getItem(storageKey) || "{}");
      const newest = [latest.current, disk.confirmed, accepted]
        .filter(
          (value): value is Confirmed =>
            !!value && validMode(value.mode) && validRevision(value.revision),
        )
        .sort((a, b) => b.revision - a.revision)[0];
      const next: Stored = {
        confirmed: newest,
        ...(disk.pending && disk.pending.request_id !== request.request_id
          ? { pending: disk.pending }
          : {}),
      };
      localStorage.setItem(storageKey, JSON.stringify(next));
      if (mounted.current) setStored(next);
      await refresh();
    } catch (cause) {
      const details =
        cause instanceof ApiError
          ? (cause.details as { outcome?: string })
          : undefined;
      if (details?.outcome === "not_applied") {
        try {
          const disk: Stored = JSON.parse(
            localStorage.getItem(storageKey) || "{}",
          );
          const next: Stored = {
            confirmed:
              disk.confirmed &&
              disk.confirmed.revision > latest.current.revision
                ? disk.confirmed
                : latest.current,
            ...(disk.pending && disk.pending.request_id !== request.request_id
              ? { pending: disk.pending }
              : {}),
          };
          localStorage.setItem(storageKey, JSON.stringify(next));
          if (mounted.current) setStored(next);
        } catch {
          // Keep the receipt until its confirmed rejection can be saved.
        }
        void refresh();
      }
      if (mounted.current) setError(errorText(cause));
    } finally {
      lock.current = false;
      if (mounted.current) setSaving(false);
    }
  };
  if (!lead.agentModeSupported) return null;
  return (
    <div
      className="agent-mode-control"
      data-agent-mode={confirmed.mode}
      data-agent-mode-revision={confirmed.revision}
    >
      <button
        type="button"
        className="agent-mode-switch"
        role="switch"
        aria-label="Multi-agent mode"
        aria-checked={confirmed.mode === "multi"}
        disabled={!workspaceId || saving || !!stored.pending || !!initial.error}
        title="Single agent: no new workers. Multi agent: the lead can start workers."
        onClick={() => void submit()}
      >
        <span className="agent-mode-track" aria-hidden="true">
          <span />
        </span>
        {confirmed.mode === "multi" ? "Multi agent" : "Single agent"}
      </button>
      {saving ? (
        <span role="status" className="agent-mode-note">
          Applying…
        </span>
      ) : stored.pending ? (
        <button
          type="button"
          className="agent-mode-retry"
          onClick={() => void submit()}
        >
          Retry mode change
        </button>
      ) : confirmed.mode === "single" ? (
        <span className="agent-mode-note">Current work can finish.</span>
      ) : null}
      {error && (
        <span className="agent-mode-error" role="alert">
          {error}
        </span>
      )}
      {initial.error && (
        <button
          type="button"
          className="agent-mode-retry"
          onClick={() => {
            try {
              localStorage.removeItem(storageKey);
              setInitial({ value: {}, error: "" });
              setStored({});
              setError("");
            } catch (cause) {
              setError(errorText(cause));
            }
          }}
        >
          Discard unreadable mode request
        </button>
      )}
    </div>
  );
}
