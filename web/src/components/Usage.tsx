import { useEffect, useState } from "react";
import { Button, Popover, Progress, Tooltip } from "@mantine/core";
import { ChevronUp, Gauge, RefreshCw, RotateCcw } from "lucide-react";
import type { Agent, Json } from "../types";
import "./Usage.css";

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
const formatPercent = (value: number) =>
  value > 0 && value < 1 ? "<1%" : `${Math.floor(value)}%`;
const duration = (minutes: unknown, fallback: string) => {
  if (!number(minutes) || minutes <= 0) return fallback;
  if (minutes % 1440 === 0) return `${minutes / 1440}d`;
  if (minutes % 60 === 0) return `${minutes / 60}h`;
  return `${minutes}m`;
};
function readBuckets(limits: Json | null, now: number): LimitBucket[] {
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
    });
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
}: {
  agent: Agent;
  limits: Json | null;
  reload: () => void;
}) {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now() / 1000), 30000);
    return () => window.clearInterval(timer);
  }, []);
  const c = agent.contextUsage;
  const known = c && number(c.tokens) && number(c.window) && c.window > 0;
  const percent = known ? Math.round((c.tokens! / c.window!) * 100) : null;
  const buckets = readBuckets(limits, now);
  const selected = selectedBucket(buckets, agent.model || "");
  const windows = selected?.windows.slice(0, 2) || [];
  const stale = windows.some((window) => window.expired);
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
            ? `${c!.tokens!.toLocaleString()} / ${c!.window!.toLocaleString()} tokens. Last reported context.`
            : "Codex has not reported context use yet."
        }
      >
        <span className="context-use" tabIndex={0}>
          <Progress
            size={3}
            w={28}
            value={Math.min(100, percent || 0)}
            color={percent !== null && percent >= 85 ? "orange" : "gray"}
            aria-label="Context used"
          />
          {percent === null ? "Context unavailable" : `Context ${percent}%`}
        </span>
      </Tooltip>
      <span className="compactions">
        <RotateCcw size={12} />
        {agent.compactions === undefined
          ? "Compactions unavailable"
          : `${agent.compactions} ${agent.compactionsObservedOnly ? "observed " : ""}compactions`}
      </span>
      <Popover
        position="top-end"
        width="min(390px, calc(100vw - 24px))"
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
              {limits?.error && (
                <span className="account-limits-warning">Update failed</span>
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
                <p>Remaining allowance</p>
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
                        <p className="account-limit-reset">
                          {window.reset
                            ? `${window.expired ? "Reset was" : "Resets"} ${new Date(window.reset * 1000).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}`
                            : "Reset time unavailable"}
                        </p>
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
            {limits?.error && (
              <p className="account-limits-error" role="status">
                Update failed: {limits.error}
              </p>
            )}
            <footer className="account-limits-updated">
              {number(limits?.at)
                ? `Updated ${new Date(limits!.at * 1000).toLocaleTimeString()}`
                : "Update time unavailable"}
            </footer>
          </section>
        </Popover.Dropdown>
      </Popover>
    </div>
  );
}
