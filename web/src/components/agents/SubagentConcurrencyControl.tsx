import { useEffect, useRef, useState } from "react";
import { api, ApiError, errorText } from "../../api";
import type { Agent } from "../../types";
import "./subagent-concurrency-control.css";

const DEFAULT_CONCURRENCY = 32;
const MIN_CONCURRENCY = 0;
const MAX_CONCURRENCY = 512;

type Confirmed = { concurrency: number; revision: number };
type LimitRequest = {
  id: string;
  subagent_concurrency: number;
  expected_mode_revision: number;
  request_id: string;
};
type LegacyModeRequest = {
  id: string;
  agent_mode: "multi" | "single";
  expected_mode_revision: number;
  request_id: string;
};
type PendingRequest = LimitRequest | LegacyModeRequest;
type Stored = { confirmed?: Confirmed; pending?: PendingRequest };
type Props = {
  lead: Agent;
  stateDir: string;
  workspaceId: string;
  refresh: () => Promise<void>;
};

const validConcurrency = (value: unknown): value is number =>
  Number.isSafeInteger(value) &&
  Number(value) >= MIN_CONCURRENCY &&
  Number(value) <= MAX_CONCURRENCY;
const validRevision = (value: unknown): value is number =>
  Number.isSafeInteger(value) && Number(value) >= 0;
const concurrencyOf = (agent: Agent): number =>
  validConcurrency(agent.concurrency) ? agent.concurrency : DEFAULT_CONCURRENCY;
const stateOf = (agent: Agent): Confirmed => ({
  concurrency: concurrencyOf(agent),
  revision: validRevision(agent.agentModeRevision)
    ? agent.agentModeRevision
    : 0,
});
const pendingTarget = (
  request: PendingRequest | undefined,
): number | undefined => {
  if (!request) return undefined;
  if ("subagent_concurrency" in request)
    return (request as LimitRequest).subagent_concurrency;
  return (request as LegacyModeRequest).agent_mode === "single"
    ? MIN_CONCURRENCY
    : DEFAULT_CONCURRENCY;
};
const validPending = (
  value: unknown,
  leadId: string,
): value is PendingRequest => {
  if (!value || typeof value !== "object") return false;
  const request = value as Record<string, unknown>;
  if (
    request.id !== leadId ||
    !validRevision(request.expected_mode_revision) ||
    typeof request.request_id !== "string" ||
    !request.request_id
  )
    return false;
  if ("subagent_concurrency" in request)
    return validConcurrency(request.subagent_concurrency);
  return request.agent_mode === "multi" || request.agent_mode === "single";
};
const validConfirmed = (value: unknown): value is Confirmed => {
  if (!value || typeof value !== "object") return false;
  const confirmed = value as Partial<Confirmed>;
  return (
    validConcurrency(confirmed.concurrency) && validRevision(confirmed.revision)
  );
};

export default function SubagentConcurrencyControl(props: Props) {
  const scope = JSON.stringify([
    props.stateDir,
    props.workspaceId,
    props.lead.id,
  ]);
  return (
    <ScopedConcurrencyControl
      key={scope}
      storageKey={`studio-agent-mode:${scope}`}
      {...props}
    />
  );
}

function ScopedConcurrencyControl({
  lead,
  workspaceId,
  storageKey,
  refresh,
}: Props & { storageKey: string }) {
  const snapshot = stateOf(lead);
  const [initial, setInitial] = useState(() => {
    try {
      const value = JSON.parse(localStorage.getItem(storageKey) || "{}") as {
        confirmed?: unknown;
        pending?: unknown;
      };
      if (
        !value ||
        typeof value !== "object" ||
        (value.confirmed &&
          !validConfirmed(value.confirmed) &&
          // A previous release stored the mode here. It must not override the
          // server's new numeric concurrency value during upgrade.
          !(
            typeof value.confirmed === "object" &&
            value.confirmed !== null &&
            ["multi", "single"].includes(
              String((value.confirmed as { mode?: unknown }).mode),
            ) &&
            validRevision((value.confirmed as { revision?: unknown }).revision)
          )) ||
        (value.pending && !validPending(value.pending, lead.id))
      )
        throw new Error("The saved concurrency request cannot be read.");
      const pending = validPending(value.pending, lead.id)
        ? value.pending
        : undefined;
      return {
        value: {
          ...(validConfirmed(value.confirmed)
            ? { confirmed: value.confirmed }
            : {}),
          ...(pending ? { pending } : {}),
        } satisfies Stored,
        error: "",
      };
    } catch (cause) {
      return { value: {} as Stored, error: errorText(cause) };
    }
  });
  const [stored, setStored] = useState(initial.value);
  const [error, setError] = useState(initial.error);
  const [saving, setSaving] = useState(false);
  const [draft, setDraft] = useState<number | string>(
    () => pendingTarget(initial.value.pending) ?? concurrencyOf(lead),
  );
  const dirty = useRef(false);
  const lock = useRef(false);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const confirmed =
    stored.confirmed && stored.confirmed.revision > snapshot.revision
      ? stored.confirmed
      : snapshot;
  const latest = useRef(confirmed);
  latest.current = confirmed;

  // Retain a newer accepted revision while snapshots catch up. Old mode-only
  // cache entries are intentionally ignored.
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
      // The server snapshot remains usable if this best-effort cache fails.
    }
  }, [
    snapshot.concurrency,
    snapshot.revision,
    stored,
    storageKey,
    initial.error,
  ]);

  useEffect(() => {
    if (dirty.current || stored.pending) return;
    setDraft(confirmed.concurrency);
  }, [confirmed.concurrency, confirmed.revision, stored.pending]);

  const submit = async () => {
    if (lock.current || initial.error || !workspaceId) return;
    const validDraft = validConcurrency(draft);
    if (!stored.pending && (!validDraft || draft === confirmed.concurrency)) {
      if (!validDraft) setError("Enter a whole number from 0 to 512.");
      else dirty.current = false;
      return;
    }
    // A saved v1 mode request is replayed byte-for-byte as its original body;
    // new requests persist the target and identity before touching the server.
    const request: PendingRequest = stored.pending ?? {
      id: lead.id,
      subagent_concurrency: draft as number,
      expected_mode_revision: confirmed.revision,
      request_id: crypto.randomUUID(),
    };
    const pending: Stored = { confirmed, pending: request };
    try {
      localStorage.setItem(storageKey, JSON.stringify(pending));
    } catch {
      setError(
        "Cannot save the concurrency request on this device. Free browser storage and retry.",
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
        !validConcurrency(result.concurrency) ||
        !validRevision(result.agentModeRevision) ||
        result.agentModeRevision < request.expected_mode_revision
      )
        throw new Error(
          "The server did not confirm the concurrency request. Retry the same request.",
        );
      const accepted = stateOf(result);
      const disk = JSON.parse(
        localStorage.getItem(storageKey) || "{}",
      ) as Stored;
      const newest = [latest.current, disk.confirmed, accepted]
        .filter(validConfirmed)
        .sort((a, b) => b.revision - a.revision)[0];
      const next: Stored = {
        confirmed: newest,
        ...(disk.pending?.request_id !== request.request_id
          ? { pending: disk.pending }
          : {}),
      };
      localStorage.setItem(storageKey, JSON.stringify(next));
      if (mounted.current) {
        setStored(next);
        dirty.current = false;
        setDraft(newest.concurrency);
      }
      await refresh();
    } catch (cause) {
      const details =
        cause instanceof ApiError
          ? (cause.details as { outcome?: string })
          : undefined;
      if (details?.outcome === "not_applied") {
        try {
          const disk = JSON.parse(
            localStorage.getItem(storageKey) || "{}",
          ) as Stored;
          const newest = [disk.confirmed, latest.current]
            .filter(validConfirmed)
            .sort((a, b) => b.revision - a.revision)[0];
          const next: Stored = {
            confirmed: newest,
            ...(disk.pending?.request_id !== request.request_id
              ? { pending: disk.pending }
              : {}),
          };
          localStorage.setItem(storageKey, JSON.stringify(next));
          if (mounted.current) {
            setStored(next);
            dirty.current = false;
            setDraft(newest.concurrency);
          }
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
  const mode = confirmed.concurrency === 0 ? "single" : "multi";
  const pending = stored.pending;
  return (
    <form
      className="agent-mode-control"
      data-agent-mode={mode}
      data-agent-mode-revision={confirmed.revision}
      data-concurrency={confirmed.concurrency}
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <label className="agent-mode-limit-label">
        <span>Subagent parallelism</span>
        <span className="agent-mode-limit-mode">
          {mode === "single" ? "Single agent" : "Multi agent"}
        </span>
        <input
          type="number"
          min={MIN_CONCURRENCY}
          max={MAX_CONCURRENCY}
          step={1}
          inputMode="numeric"
          aria-label="Subagent parallelism"
          value={draft}
          disabled={!workspaceId || saving || !!pending || !!initial.error}
          onChange={(event) => {
            const value = event.currentTarget.valueAsNumber;
            dirty.current = true;
            setDraft(Number.isNaN(value) ? event.currentTarget.value : value);
            setError("");
          }}
        />
      </label>
      <button
        type="submit"
        className="agent-mode-apply"
        disabled={!workspaceId || saving || !!pending || !!initial.error}
      >
        Apply
      </button>
      {saving ? (
        <span role="status" className="agent-mode-note">
          Applying…
        </span>
      ) : pending ? (
        <button
          type="button"
          className="agent-mode-retry"
          onClick={() => void submit()}
        >
          {"subagent_concurrency" in pending
            ? "Retry limit change"
            : "Retry mode change"}
        </button>
      ) : confirmed.concurrency === 0 ? (
        <span className="agent-mode-note">No new worker turns will start.</span>
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
              dirty.current = false;
              setDraft(concurrencyOf(lead));
              setError("");
            } catch (cause) {
              setError(errorText(cause));
            }
          }}
        >
          Discard unreadable concurrency request
        </button>
      )}
    </form>
  );
}
