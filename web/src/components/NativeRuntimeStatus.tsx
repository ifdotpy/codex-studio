import { useEffect, useState } from "react";
import { api, errorText } from "../api";
import type { Account } from "./Accounts";
import "./native-runtime-status.css";

interface RuntimeAccount {
  status: "current" | "waiting" | "updating" | "failed";
  version?: string;
  pid?: number;
  targetVersion?: string;
  reason?: string;
  updatedAt?: number;
}

interface RuntimeStatus {
  status: "checking" | "ready" | "failed";
  checkedAt: number | null;
  error?: string;
  selected: { version: string; sourcePath: string; sha256: string } | null;
  candidates: {
    version: string;
    path: string;
    status: "approved" | "rejected";
    error?: string;
  }[];
  accounts: Record<string, RuntimeAccount>;
}

const accountStatus = {
  current: "Up to date",
  waiting: "Waiting",
  updating: "Updating",
  failed: "Update failed",
};

export default function NativeRuntimeStatus({
  opened,
  accounts,
}: {
  opened: boolean;
  accounts: Account[];
}) {
  const [data, setData] = useState<RuntimeStatus | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    setData(null);
    setError("");
    if (!opened) return;
    let live = true;
    let timer: number | undefined;
    const controller = new AbortController();
    const refresh = async () => {
      try {
        const result = await api<{ nativeRuntime?: RuntimeStatus }>(
          "/api/desktop",
          undefined,
          { signal: controller.signal },
        );
        if (live) {
          setData(result.nativeRuntime ?? null);
          setError("");
        }
      } catch (failure) {
        if (live) setError(errorText(failure));
      } finally {
        if (live) timer = window.setTimeout(refresh, 10000);
      }
    };
    void refresh();
    return () => {
      live = false;
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [opened]);

  if (!opened || !data) return null;
  const entries = Object.entries(data.accounts);
  const pending = entries.filter(
    ([, account]) =>
      account.status === "waiting" || account.status === "updating",
  ).length;
  const rejected = data.candidates.filter(
    (candidate) => candidate.status === "rejected",
  );
  return (
    <section className="native-runtime-status" aria-label="Codex runtime">
      <div className="native-runtime-heading">
        <strong>Codex runtime</strong>
        {data.selected && <span>{data.selected.version}</span>}
      </div>
      <p className="native-runtime-summary" role="status">
        {data.status === "checking"
          ? "Checking installed versions…"
          : data.status === "failed"
            ? "Runtime check failed"
            : "Automatic updates"}
        {pending > 0 &&
          ` · ${pending} account${pending === 1 ? "" : "s"} pending`}
      </p>
      {data.checkedAt != null && data.checkedAt > 0 && (
        <time dateTime={new Date(data.checkedAt * 1000).toISOString()}>
          Checked {new Date(data.checkedAt * 1000).toLocaleString()}
        </time>
      )}
      {data.error && <p className="native-runtime-error">{data.error}</p>}
      {error && <p className="native-runtime-error">{error}</p>}
      {entries.length > 0 && (
        <ul className="native-runtime-accounts">
          {entries.map(([key, runtime]) => {
            const account = accounts.find((item) => item.id === key);
            return (
              <li key={key}>
                <div className="native-runtime-account-heading">
                  <span>{account?.label || account?.email || key}</span>
                  <span
                    className={
                      runtime.status === "failed"
                        ? "native-runtime-error"
                        : undefined
                    }
                  >
                    {accountStatus[runtime.status]}
                    {runtime.version && ` · ${runtime.version}`}
                  </span>
                </div>
                {runtime.targetVersion && runtime.status !== "current" && (
                  <small>Target: {runtime.targetVersion}</small>
                )}
                {runtime.reason && <small>{runtime.reason}</small>}
              </li>
            );
          })}
        </ul>
      )}
      {rejected.map((candidate) => (
        <p
          className="native-runtime-error"
          key={`${candidate.path}:${candidate.version}`}
        >
          Version {candidate.version || "unknown"} rejected
          {candidate.error ? `: ${candidate.error}` : "."}
        </p>
      ))}
    </section>
  );
}
