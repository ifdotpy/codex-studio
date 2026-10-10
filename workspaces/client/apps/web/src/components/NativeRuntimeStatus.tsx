import { localDateTime } from "../local-time";
import { useEffect, useState } from "react";
import { get, type GetResult, errorText } from "../api";
import type { Account } from "./Accounts";
import { watchResourceReads } from "./watchResourceReads";
import "./native-runtime-status.css";

type DesktopStatus = GetResult<"/api/desktop">;
type RuntimeStatus = NonNullable<DesktopStatus["nativeRuntime"]>;
type ProviderVersions = NonNullable<DesktopStatus["providerVersions"]>;

const accountStatus = {
  current: "Up to date",
  waiting: "Waiting",
  updating: "Updating",
  failed: "Update failed",
};

const providerStatus = {
  checking: "Checking",
  current: "Running version meets the tested baseline",
  outdated: "Below the tested baseline",
  unknown: "Running version unknown",
  error: "Version check failed",
};

export default function NativeRuntimeStatus({
  opened,
  accounts,
}: {
  opened: boolean;
  accounts: Account[];
}) {
  const [data, setData] = useState<RuntimeStatus | null>(null);
  const [providers, setProviders] = useState<ProviderVersions>({
    checkedAt: null,
    providers: [],
    warnings: [],
  });
  const [error, setError] = useState("");
  useEffect(() => {
    setData(null);
    setProviders({ checkedAt: null, providers: [], warnings: [] });
    setError("");
    if (!opened) return;
    let live = true;
    const controller = new AbortController();
    const stop = watchResourceReads(
      { kind: "desktop" },
      async () => {
        const result = await get("/api/desktop", {
          signal: controller.signal,
        });
        if (live) {
          setData(result.nativeRuntime ?? null);
          setProviders(
            result.providerVersions || {
              checkedAt: null,
              providers: [],
              warnings: [],
            },
          );
          setError("");
        }
      },
      (failure) => {
        if (live) setError(errorText(failure));
      },
    );
    return () => {
      live = false;
      controller.abort();
      stop();
    };
  }, [opened]);

  if (!opened) return null;
  const entries = Object.entries(data?.accounts || {});
  const pending = entries.filter(
    ([, account]) =>
      account.status === "waiting" || account.status === "updating",
  ).length;
  const reasonCount = (reason: string) =>
    entries.filter(([, runtime]) => runtime.reason === reason).length;
  const rejected = data?.candidates.filter(
    (candidate) => candidate.status === "rejected",
  );
  return (
    <section
      className="accounts-runtime"
      aria-label="Runtime and provider versions"
    >
      <h3>Runtime and provider versions</h3>
      {(error ||
        data?.status === "failed" ||
        data?.error ||
        rejected?.length ||
        entries.some(([, runtime]) => runtime.status === "failed") ||
        providers.providers.some(
          (provider) =>
            provider.status === "error" ||
            provider.status === "outdated" ||
            provider.status === "unknown" ||
            !!provider.error,
        )) && (
        <p role="status" className="native-runtime-error">
          Some runtime checks need attention. See the details below.
        </p>
      )}
      <details>
        <summary>Version details</summary>
        {data && (
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
                Checked {localDateTime(new Date(data.checkedAt * 1000))}
              </time>
            )}
            {data.error && <p className="native-runtime-error">{data.error}</p>}
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
                      {runtime.reason && reasonCount(runtime.reason) === 1 && (
                        <small>{runtime.reason}</small>
                      )}
                      {runtime.targetVersion &&
                        runtime.status !== "current" && (
                          <small>Target: {runtime.targetVersion}</small>
                        )}
                    </li>
                  );
                })}
              </ul>
            )}
            {[
              ...new Set(
                entries
                  .map(([, runtime]) => runtime.reason)
                  .filter(
                    (reason): reason is string =>
                      !!reason && reasonCount(reason) > 1,
                  ),
              ),
            ].map((reason) => (
              <p key={reason}>{reason}</p>
            ))}
            {rejected?.map((candidate) => (
              <p
                className="native-runtime-error"
                key={`${candidate.path}:${candidate.version}`}
              >
                Version {candidate.version || "unknown"} rejected
                {candidate.error ? `: ${candidate.error}` : "."}
              </p>
            ))}
          </section>
        )}
        <section
          className="native-runtime-status provider-version-status"
          aria-label="Provider versions"
        >
          <div className="native-runtime-heading">
            <strong>Provider CLI versions</strong>
            {providers.checkedAt != null && providers.checkedAt > 0 && (
              <time
                dateTime={new Date(providers.checkedAt * 1000).toISOString()}
              >
                Checked {localDateTime(new Date(providers.checkedAt * 1000))}
              </time>
            )}
          </div>
          {error && <p className="native-runtime-error">{error}</p>}
          {providers.providers.length === 0 ? (
            <p className="native-runtime-summary" role="status">
              {providers.checkedAt == null
                ? "Checking connected provider CLIs…"
                : "No connected provider CLIs were found."}
            </p>
          ) : (
            <ul className="native-runtime-accounts">
              {providers.providers.map((provider) => {
                const account = accounts.find(
                  (item) => item.id === provider.accountKey,
                );
                const label =
                  provider.provider === "claude"
                    ? "Claude Code"
                    : provider.provider === "codex"
                      ? "Codex CLI"
                      : provider.provider;
                return (
                  <li key={provider.id}>
                    <div className="native-runtime-account-heading">
                      <span>
                        {label} ·{" "}
                        {account?.label ||
                          account?.email ||
                          provider.accountKey}
                      </span>
                      <span
                        className={
                          provider.status === "error"
                            ? "native-runtime-error"
                            : provider.status === "outdated"
                              ? "native-runtime-warning"
                              : undefined
                        }
                      >
                        {providerStatus[provider.status]}
                      </span>
                    </div>
                    <small>
                      Running version: {provider.runningVersion || "Unknown"}
                    </small>
                    <small>
                      Installed version:{" "}
                      {provider.installedVersion || "Unknown"}
                    </small>
                    {provider.configuredVersion &&
                      provider.configuredVersion !==
                        provider.installedVersion && (
                        <small>
                          Current profile executable:{" "}
                          {provider.configuredVersion}
                        </small>
                      )}
                    <small>
                      Tested minimum: {provider.baseline || "Not established"}
                    </small>
                    {provider.error && (
                      <small className="native-runtime-error">
                        {provider.error}
                      </small>
                    )}
                    {provider.message && provider.status !== "current" && (
                      <small
                        className={
                          provider.status === "outdated"
                            ? "native-runtime-warning"
                            : undefined
                        }
                      >
                        {provider.message}
                      </small>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
        </section>
      </details>
    </section>
  );
}
