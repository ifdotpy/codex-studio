import { useEffect, useRef, useState } from "react";
import { Button, Popover, Progress, Tooltip } from "@mantine/core";
import { ChevronUp, Gauge, RefreshCw, RotateCcw } from "lucide-react";
import type { Agent, Json } from "../types";
import { api, errorText } from "../api";
import { limitRecovery } from "../limitRecovery";
import LimitRecoveryNotice from "./LimitRecoveryNotice";
export { limitRecovery } from "../limitRecovery";
import "./Usage.css";
import Analytics from "./Analytics";

type LimitWindow = {
  label: string;
  remaining: number | null;
  reset: number | null;
  expired: boolean;
};
type LimitBucket = {
  id: string;
  name: string;
  data: Json;
  windows: LimitWindow[];
};
const number = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value);
const percentage = (value: unknown) =>
  number(value) && value >= 0 && value <= 100 ? value : null;
export const formatPercent = (value: number) =>
  value > 0 && value < 1 ? "<1%" : `${Math.floor(value)}%`;
const duration = (minutes: unknown, fallback: string) => {
  if (!number(minutes) || minutes <= 0) return fallback;
  if (minutes % 1440 === 0) return `${minutes / 1440}d`;
  if (minutes % 60 === 0) return `${minutes / 60}h`;
  return `${minutes}m`;
};
export function readBuckets(limits: Json | null, now: number): LimitBucket[] {
  const reported = limits?.data?.rateLimitsByLimitId;
  const buckets =
    reported && Object.keys(reported).length
      ? reported
      : limits?.data?.rateLimits
        ? {
            [limits.data.rateLimits.limitId || "codex"]: limits.data.rateLimits,
          }
        : {};
  return Object.entries(buckets)
    .filter(([, data]) => data && typeof data === "object")
    .map(([id, value]) => {
      const data = value as Json;
      const windows = ["primary", "secondary"].flatMap((key) => {
        const window = data[key];
        if (!window || typeof window !== "object") return [];
        const used = percentage(window.usedPercent);
        const reset =
          number(window.resetsAt) && window.resetsAt > 0
            ? window.resetsAt
            : null;
        return [
          {
            label: duration(
              window.windowDurationMins,
              key === "primary" ? "Primary" : "Secondary",
            ),
            remaining: used === null ? null : 100 - used,
            reset,
            expired: reset !== null && reset <= now,
          },
        ];
      });
      if (data.individualLimit)
        windows.push({
          label: "Individual",
          remaining: percentage(data.individualLimit.remainingPercent),
          reset: null,
          expired: false,
        });
      return {
        id,
        name: data.limitName || (id === "codex" ? "Codex" : id),
        data,
        windows,
      };
    })
    .sort((a, b) => {
      const rank = (bucket: LimitBucket) =>
        bucket.id === "codex" || bucket.data.limitId === "codex"
          ? 0
          : /spark/i.test(bucket.name)
            ? 2
            : 1;
      return rank(a) - rank(b) || a.name.localeCompare(b.name);
    });
}
const dollars = (value: unknown) =>
  number(value)
    ? new Intl.NumberFormat(undefined, {
        style: "currency",
        currency: "USD",
        maximumFractionDigits: 2,
      }).format(value)
    : "Unavailable";
function resetIn(reset: number, now: number) {
  const minutes = Math.max(0, Math.ceil((reset - now) / 60));
  if (!minutes) return "Awaiting update";
  if (minutes < 60) return `in ${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `in ${hours}h ${minutes % 60}m`;
  return `in ${Math.floor(hours / 24)}d ${hours % 24}h`;
}
function selectedBucket(buckets: LimitBucket[], model: string) {
  const matches = buckets.filter((bucket) =>
    [bucket.id, bucket.data.limitId, bucket.data.limitName].some(
      (name) =>
        typeof name === "string" && name.toLowerCase() === model.toLowerCase(),
    ),
  );
  return (
    matches[0] ||
    buckets.find(
      (bucket) => bucket.id === "codex" || bucket.data.limitId === "codex",
    ) ||
    null
  );
}
export default function Usage({
  agent,
  limits,
  reload,
  opened,
  onChange,
}: {
  agent: Agent;
  limits: Json | null;
  reload: () => void;
  opened?: boolean;
  onChange?: (opened: boolean) => void;
}) {
  const [localOpened, setLocalOpened] = useState(false);
  const limitsOpened = opened ?? localOpened;
  const changeOpened = (value: boolean) => {
    setLocalOpened(value);
    onChange?.(value);
  };
  const [analyticsOpen, setAnalyticsOpen] = useState(false);
  const [confirmReset, setConfirmReset] = useState<string | null>(null);
  const [resetPending, setResetPending] = useState(false);
  const [resetError, setResetError] = useState("");
  const [resetNotice, setResetNotice] = useState("");
  const [resetApplied, setResetApplied] = useState<string[]>([]);
  const resetLock = useRef(false);
  const resetAttempts = useRef(new Map<string, string>());
  const applyReset = async (creditId: string) => {
    if (resetLock.current || resetApplied.includes(creditId)) return;
    resetLock.current = true;
    setResetPending(true);
    setResetError("");
    setResetNotice("");
    try {
      const key = `${limits?.data?.accountId}:${creditId}`;
      const requestId = resetAttempts.current.get(key) || crypto.randomUUID();
      resetAttempts.current.set(key, requestId);
      const result = await api("/api/limits/reset", {
        credit_id: creditId,
        account_id: limits?.data?.accountId,
        account_key: agent.accountKey || "default",
        request_id: requestId,
      });
      if (
        ["reset", "alreadyRedeemed", "nothingToReset", "noCredit"].includes(
          result.outcome,
        )
      )
        resetAttempts.current.delete(key);
      if (result.outcome === "reset" || result.outcome === "alreadyRedeemed") {
        setResetApplied((ids) => [...ids, creditId]);
        setConfirmReset(null);
        setResetNotice(
          result.outcome === "reset"
            ? "Reset applied. Allowance updates from Codex."
            : "This reset credit was already used.",
        );
      } else if (result.outcome === "nothingToReset") {
        setConfirmReset(null);
        setResetNotice("No limits need a reset.");
      } else if (result.outcome === "noCredit") {
        setResetError("This reset credit is no longer available.");
      } else if (result.outcome === "uncertain") {
        setResetError(
          result.error ||
            "Reset result uncertain. Refresh limits before trying again.",
        );
      } else {
        throw new Error(
          "Reset result unavailable. Refresh limits before trying again.",
        );
      }
      reload();
    } catch (error) {
      setResetError(errorText(error));
    } finally {
      resetLock.current = false;
      setResetPending(false);
    }
  };
  const [costs, setCosts] = useState<Json | null>(null);
  useEffect(() => {
    let active = true;
    let timer: number;
    const load = async () => {
      try {
        const result = await api<Json>("/api/costs");
        if (!active) return;
        setCosts(result);
        timer = window.setTimeout(load, result.refreshing ? 1500 : 60000);
      } catch {
        if (active) {
          setCosts((previous) => ({
            ...previous,
            error: "Local costs unavailable",
          }));
          timer = window.setTimeout(load, 60000);
        }
      }
    };
    void load();
    return () => {
      active = false;
      window.clearTimeout(timer);
    };
  }, []);
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now() / 1000), 30000);
    return () => window.clearInterval(timer);
  }, []);
  const resetCredits = limits?.data?.rateLimitResetCredits;
  const resetCount = number(resetCredits?.availableCount)
    ? resetCredits.availableCount
    : null;
  const availableResets: Json[] = Array.isArray(resetCredits?.credits)
    ? resetCredits.credits.filter(
        (credit: Json) =>
          credit &&
          typeof credit.id === "string" &&
          credit.status === "available" &&
          (!number(credit.expiresAt) || credit.expiresAt > now),
      )
    : [];
  const c = agent.contextUsage;
  const known = c && number(c.tokens) && number(c.window) && c.window > 0;
  const percent = known ? Math.round((c.tokens! / c.window!) * 100) : null;
  const buckets = readBuckets(limits, now);
  const selected = selectedBucket(buckets, agent.model || "");
  const windows = selected?.windows.slice(0, 2) || [];
  const stale = windows.some((window) => window.expired);
  const recovery = limitRecovery(agent, limits, now);
  const summary = windows.length
    ? windows
        .map(
          (window) =>
            `${window.label} ${window.expired ? "refresh needed" : window.remaining === null ? "unavailable" : `${formatPercent(window.remaining)} left`}`,
        )
        .join(" · ")
    : buckets.length
      ? `${buckets.length} ${buckets.length === 1 ? "pool" : "pools"}`
      : "Unavailable";
  return (
    <div className="usage-footer" id="usage-footer">
      <Tooltip
        label={
          known
            ? `${c!.tokens!.toLocaleString()} / ${c!.window!.toLocaleString()} tokens. Last reported context. Click for analytics.`
            : "Codex has not reported context use yet. Click for analytics."
        }
      >
        <button
          type="button"
          className="context-use"
          aria-label="Open context analytics"
          onClick={() => setAnalyticsOpen(true)}
        >
          <Progress
            size={3}
            w={28}
            value={Math.min(100, percent || 0)}
            color={percent !== null && percent >= 85 ? "orange" : "gray"}
            aria-label="Context used"
          />
          {percent === null ? "Context unavailable" : `Context ${percent}%`}
        </button>
      </Tooltip>
      {analyticsOpen && (
        <Analytics
          key={agent.id}
          agent={agent}
          onClose={() => setAnalyticsOpen(false)}
        />
      )}
      <span className="compactions">
        <RotateCcw size={12} />
        {agent.compactions === undefined
          ? "Compactions unavailable"
          : `${agent.compactions} ${agent.compactionsObservedOnly ? "observed " : ""}compactions`}
      </span>
      <Popover
        opened={limitsOpened}
        onChange={changeOpened}
        position="top-end"
        width="min(370px, calc(100vw - 24px))"
        withArrow
        shadow="lg"
        trapFocus
      >
        <Popover.Target>
          <Button
            className="limits-toggle account-limits-toggle"
            variant="subtle"
            size="compact-xs"
            aria-label="Account limits"
            onClick={() => changeOpened(!limitsOpened)}
            leftSection={<Gauge size={13} />}
            rightSection={<ChevronUp size={12} />}
          >
            <span className="account-limits-summary">
              <span className="account-limits-label">Limits</span>
              <span
                className={
                  limits?.error || stale ? "account-limits-warning" : ""
                }
              >
                {summary}
              </span>
              {resetCount !== null && resetCount > 0 && (
                <span className="account-reset-summary">
                  {resetCount} resets
                </span>
              )}
              {number(costs?.data?.todayUSD) && (
                <span
                  className="account-cost-summary"
                  title="API cost estimate for all local chats"
                >
                  ≈{dollars(costs?.data?.todayUSD)} today
                </span>
              )}
            </span>
          </Button>
        </Popover.Target>
        <Popover.Dropdown className="account-limits-popover">
          <section
            className="account-limits-panel"
            aria-label="Account limits details"
          >
            <header className="account-limits-heading">
              <div>
                <h3>Account limits</h3>
                <p>Allowance left · local time</p>
              </div>
              <Button
                size="compact-xs"
                variant="subtle"
                onClick={reload}
                leftSection={<RefreshCw size={13} />}
              >
                Refresh
              </Button>
            </header>
            {recovery && (
              <LimitRecoveryNotice
                key={JSON.stringify(recovery)}
                recovery={recovery}
              />
            )}
            <div className="account-limits-groups">
              {buckets.map((bucket) => (
                <section
                  className="account-limit-group"
                  key={bucket.id}
                  aria-label={`${bucket.name} limits`}
                >
                  <header>
                    <strong>{bucket.name}</strong>
                    {bucket.data.planType && (
                      <span>{bucket.data.planType}</span>
                    )}
                  </header>
                  <div className="account-limit-windows">
                    {bucket.windows.map((window, index) => (
                      <div
                        className="account-limit-window"
                        key={`${window.label}-${index}`}
                      >
                        <div className="account-limit-value">
                          <span>{window.label}</span>
                          <strong
                            className={
                              window.remaining !== null &&
                              window.remaining <= 15 &&
                              !window.expired
                                ? "account-limits-warning"
                                : ""
                            }
                          >
                            {window.expired ? (
                              "Awaiting update"
                            ) : window.remaining === null ? (
                              "Unavailable"
                            ) : (
                              <>
                                {formatPercent(window.remaining)}
                                <small> left</small>
                              </>
                            )}
                          </strong>
                        </div>
                        {window.remaining !== null && !window.expired && (
                          <Progress
                            value={window.remaining}
                            size={5}
                            radius="xl"
                            color={window.remaining <= 15 ? "orange" : "teal"}
                            aria-label={`${bucket.name} ${window.label} remaining`}
                          />
                        )}
                        <div className="account-limit-reset">
                          {window.reset ? (
                            <>
                              <span>
                                {window.expired
                                  ? "Reset passed"
                                  : `Resets ${resetIn(window.reset, now)}`}
                              </span>
                              <time
                                dateTime={new Date(
                                  window.reset * 1000,
                                ).toISOString()}
                                title={new Date(window.reset * 1000).toString()}
                              >
                                {new Date(window.reset * 1000).toLocaleString(
                                  undefined,
                                  {
                                    month: "short",
                                    day: "numeric",
                                    hour: "2-digit",
                                    minute: "2-digit",
                                  },
                                )}
                              </time>
                            </>
                          ) : (
                            <span>Reset time unavailable</span>
                          )}
                        </div>
                      </div>
                    ))}
                  </div>
                  {!bucket.windows.length && (
                    <p className="account-limits-empty">
                      No limit windows reported.
                    </p>
                  )}
                  {bucket.data.credits && (
                    <div className="account-limit-credits">
                      <span>Credits</span>
                      <strong>
                        {bucket.data.credits.unlimited
                          ? "Unlimited"
                          : bucket.data.credits.balance != null
                            ? String(bucket.data.credits.balance)
                            : bucket.data.credits.hasCredits === true
                              ? "Available"
                              : bucket.data.credits.hasCredits === false
                                ? "None"
                                : "Unavailable"}
                      </strong>
                    </div>
                  )}
                </section>
              ))}
              {!buckets.length && (
                <p className="account-limits-empty">
                  Codex has not supplied account limits.
                </p>
              )}
            </div>
            {resetCredits && (
              <section
                className="account-reset-credits"
                aria-label="Limit reset credits"
              >
                <header>
                  <strong>Limit resets</strong>
                  <span>
                    {resetCount === null
                      ? "Count unavailable"
                      : `${resetCount} available`}
                  </span>
                </header>
                {availableResets.map((credit) => (
                  <div className="account-reset-credit" key={credit.id}>
                    <div className="account-reset-credit-row">
                      <div>
                        <strong title={credit.description || undefined}>
                          {credit.title || "Reset credit"}
                        </strong>
                        <span>
                          {number(credit.expiresAt) ? (
                            <>
                              Expires {resetIn(credit.expiresAt, now)} ·{" "}
                              <time
                                dateTime={new Date(
                                  credit.expiresAt * 1000,
                                ).toISOString()}
                              >
                                {new Date(
                                  credit.expiresAt * 1000,
                                ).toLocaleString(undefined, {
                                  month: "short",
                                  day: "numeric",
                                  hour: "2-digit",
                                  minute: "2-digit",
                                })}
                              </time>
                            </>
                          ) : (
                            "Expiration unavailable"
                          )}
                        </span>
                      </div>
                      <Button
                        size="compact-xs"
                        variant="light"
                        disabled={
                          resetPending ||
                          resetApplied.includes(credit.id) ||
                          credit.resetType !== "codexRateLimits" ||
                          !limits?.data?.accountId
                        }
                        onClick={() => {
                          setConfirmReset(credit.id);
                          setResetError("");
                        }}
                      >
                        {resetApplied.includes(credit.id)
                          ? "Applied"
                          : "Apply reset"}
                      </Button>
                    </div>
                    {confirmReset === credit.id && (
                      <div
                        className="account-reset-confirm"
                        role="group"
                        aria-label={`Confirm ${credit.title || "limit reset"}`}
                      >
                        <p>
                          Use this reset credit now? This spends one credit.
                        </p>
                        <div>
                          <Button
                            size="compact-xs"
                            variant="default"
                            disabled={resetPending}
                            onClick={() => {
                              setConfirmReset(null);
                              setResetError("");
                            }}
                          >
                            Cancel
                          </Button>
                          <Button
                            size="compact-xs"
                            loading={resetPending}
                            disabled={resetPending}
                            onClick={() => void applyReset(credit.id)}
                          >
                            Use one reset credit
                          </Button>
                        </div>
                      </div>
                    )}
                  </div>
                ))}
                {!availableResets.length && (
                  <p>
                    {resetCount === 0
                      ? "No reset credits available."
                      : "Reset credit details unavailable. Refresh to check."}
                  </p>
                )}
                {resetError && (
                  <p className="account-limits-warning" role="alert">
                    {resetError}
                  </p>
                )}
                {resetNotice && <p role="status">{resetNotice}</p>}
              </section>
            )}
            <section
              className="account-costs"
              aria-label="Local cost estimates"
            >
              <header>
                <strong>API cost estimate</strong>
                <span>USD</span>
              </header>
              <div className="account-cost-values">
                <div>
                  <span>Today</span>
                  <strong>{dollars(costs?.data?.todayUSD)}</strong>
                </div>
                <div>
                  <span>Last 30 days</span>
                  <strong>{dollars(costs?.data?.last30DaysUSD)}</strong>
                </div>
              </div>
              <p className="account-cost-caption">
                All local chats
                {costs?.data?.coverage === "partial" && " · Partial estimate"}
                {(costs?.stale || costs?.error) &&
                  (number(costs?.data?.todayUSD) ||
                  number(costs?.data?.last30DaysUSD)
                    ? " · Saved estimate"
                    : " · Estimate unavailable")}
              </p>
            </section>
            <footer className="account-limits-updated" role="status">
              {limits?.error
                ? limits?.data
                  ? "Saved limits"
                  : "Limits temporarily unavailable"
                : number(limits?.at)
                  ? "Updated"
                  : ""}
              {number(limits?.at) &&
                ` · ${new Date(limits!.at * 1000).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })}`}
            </footer>
          </section>
        </Popover.Dropdown>
      </Popover>
    </div>
  );
}
