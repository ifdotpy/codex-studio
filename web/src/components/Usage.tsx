import { localDateTime, localTime } from "../local-time";
import { accountLimits } from "../usage/accountUsage";
import { weeklyRunway } from "../usage/weeklyRunway";
import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { Button, Popover, Progress, Tabs, Tooltip } from "@mantine/core";
import { ChevronUp, ExternalLink, Gauge, RefreshCw } from "lucide-react";
import type { Agent, Json, JsonValue } from "../types";
import { errorText, get, post, type GetResult } from "../api";
import { peekSessionCost, storeSessionCost } from "../usage/sessionCostCache";
import { useRecoveredLimit } from "./useRecoveredLimit";
import { limitRecovery } from "../usage/limitRecovery";
import LimitRecoveryNotice from "./LimitRecoveryNotice";
import { watchResourceReads } from "./watchResourceReads";
export { limitRecovery } from "../usage/limitRecovery";
import "./Usage.css";
import TokenRate from "./TokenRate";
import {
  AccountTilesLayout,
  AccountRemainingBar,
  remainingLimit,
} from "./AccountTiles";
import { accountDisplayName } from "../accountName";
const Analytics = lazy(() => import("./Analytics"));

export type UsageAccount = {
  key: string;
  label: string;
  email?: string | null;
  provider?: string;
  accountId?: string | null;
  signedOut?: boolean;
  limits: Json | null;
  loading: boolean;
  reload: (force?: boolean) => void | Promise<void>;
};

const humanAccountLabel = (account?: UsageAccount) =>
  account?.email ||
  (account?.label && account.label !== account.key ? account.label : "") ||
  "Account name unavailable";

const displayAccountName = (account?: UsageAccount) =>
  humanAccountLabel(account) === "Account name unavailable"
    ? "Account name unavailable"
    : accountDisplayName({
        label: account?.label === account?.key ? "" : account?.label || "",
        email: account?.email,
      });
const accountProvider = (account?: UsageAccount) =>
  account?.provider === "claude" ? "claude" : "codex";

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
type AvailableReset = {
  id: string;
  title?: string;
  description?: string;
  expiresAt?: number;
  resetType?: string;
};
const number = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value);
const jsonObject = (value: JsonValue | null | undefined): Json | null =>
  value && typeof value === "object" && !Array.isArray(value) ? value : null;
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
  const responseData = jsonObject(limits?.data);
  const reported = jsonObject(responseData?.rateLimitsByLimitId);
  const rateLimits = jsonObject(responseData?.rateLimits);
  const buckets =
    reported && Object.keys(reported).length
      ? reported
      : rateLimits
        ? {
            [typeof rateLimits.limitId === "string" && rateLimits.limitId
              ? rateLimits.limitId
              : "codex"]: rateLimits,
          }
        : {};
  return Object.entries(buckets)
    .flatMap(([id, value]) => {
      const data = jsonObject(value);
      if (!data) return [];
      const windows = ["primary", "secondary"].flatMap((key) => {
        const window = jsonObject(data[key]);
        if (!window) return [];
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
      const individualLimit = jsonObject(data.individualLimit);
      if (individualLimit)
        windows.push({
          label: "Individual",
          remaining: percentage(individualLimit.remainingPercent),
          reset: null,
          expired: false,
        });
      return {
        id,
        name:
          (typeof data.limitName === "string" && data.limitName) ||
          (id === "codex" ? "Codex" : id === "claude" ? "Claude" : id),
        data,
        windows,
      };
    })
    .sort((a, b) => {
      const rank = (bucket: LimitBucket) =>
        ["codex", "claude"].includes(bucket.id) ||
        (typeof bucket.data.limitId === "string" &&
          ["codex", "claude"].includes(bucket.data.limitId))
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
export default function Usage({
  agent,
  stateDir,
  limits: reportedLimits,
  accountLabel,
  reload,
  opened,
  onChange,
  limitsLoading,
  accounts = [],
}: {
  agent: Agent;
  stateDir: string;
  limits: Json | null;
  accountLabel?: string;
  reload: () => void | Promise<void>;
  opened?: boolean;
  onChange?: (opened: boolean) => void;
  limitsLoading?: boolean;
  accounts?: UsageAccount[];
}) {
  const [localOpened, setLocalOpened] = useState(false);
  const limitsOpened = opened ?? localOpened;
  const changeOpened = (value: boolean) => {
    setLocalOpened(value);
    onChange?.(value);
  };
  const fallbackKey = agent.accountKey || "default";
  const usageAccounts = accounts.length
    ? accounts
    : [
        {
          key: fallbackKey,
          label: accountLabel || "",
          limits: reportedLimits,
          loading: limitsLoading ?? !reportedLimits,
          reload: () => reload(),
        },
      ];
  const [activeAccountKey, setActiveAccountKey] = useState(fallbackKey);
  const activeAccount =
    usageAccounts.find((item) => item.key === activeAccountKey) ||
    usageAccounts[0];
  const selectedAccountKey = activeAccount?.key || fallbackKey;
  const activeAccountReload = useRef(activeAccount?.reload);
  activeAccountReload.current = activeAccount?.reload;
  const autoReloadedAccounts = useRef(new Set<string>());
  const limits = accountLimits(
    activeAccount?.limits,
    selectedAccountKey,
    activeAccount?.accountId,
  );
  const limitsData = jsonObject(limits?.data);
  const loadingLimits =
    activeAccount?.loading ?? limitsLoading ?? !reportedLimits;
  const accountAgent = { ...agent, accountKey: selectedAccountKey };
  const [analyticsOpen, setAnalyticsOpen] = useState(false);
  const [contextOpen, setContextOpen] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const refreshLimits = async () => {
    if (refreshing) return;
    setRefreshing(true);
    try {
      await (activeAccount?.reload(true) ?? reload());
    } finally {
      setRefreshing(false);
    }
  };
  const [confirmReset, setConfirmReset] = useState<string | null>(null);
  const [resetPending, setResetPending] = useState(false);
  const [resetError, setResetError] = useState("");
  const [resetNotice, setResetNotice] = useState("");
  const [resetApplied, setResetApplied] = useState<string[]>([]);
  const resetAppliedKey = (creditId: string) =>
    `${selectedAccountKey}:${creditId}`;
  useEffect(() => {
    setConfirmReset(null);
    setResetError("");
    setResetNotice("");
  }, [selectedAccountKey]);
  const resetLock = useRef(false);
  const resetAttempts = useRef(new Map<string, string>());
  const applyReset = async (creditId: string) => {
    if (resetLock.current || resetApplied.includes(resetAppliedKey(creditId)))
      return;
    resetLock.current = true;
    setResetPending(true);
    setResetError("");
    setResetNotice("");
    try {
      const accountId = limitsData?.accountId;
      if (typeof accountId !== "string")
        throw new Error("Refresh limits before using this reset credit.");
      const key = `${accountId}:${creditId}`;
      const requestId = resetAttempts.current.get(key) || crypto.randomUUID();
      resetAttempts.current.set(key, requestId);
      const result = await post("/api/limits/reset", {
        credit_id: creditId,
        account_id: accountId,
        account_key: selectedAccountKey,
        request_id: requestId,
      });
      switch (result.outcome) {
        case "reset":
        case "alreadyRedeemed":
          resetAttempts.current.delete(key);
          setResetApplied((ids) => [...ids, resetAppliedKey(creditId)]);
          setConfirmReset(null);
          setResetNotice(
            result.outcome === "reset"
              ? "Reset applied."
              : "This reset credit was already used.",
          );
          break;
        case "nothingToReset":
          resetAttempts.current.delete(key);
          setConfirmReset(null);
          setResetNotice("No limits need a reset.");
          break;
        case "noCredit":
          resetAttempts.current.delete(key);
          setResetError("This reset credit is no longer available.");
          break;
        case "uncertain":
          setResetError(
            result.error ||
              "Reset result uncertain. Refresh limits before trying again.",
          );
          break;
      }
      void (activeAccount?.reload(true) ?? reload());
    } catch (error) {
      setResetError(errorText(error));
    } finally {
      resetLock.current = false;
      setResetPending(false);
    }
  };
  const accountKey = selectedAccountKey;
  const [costs, setCosts] = useState<
    (Partial<GetResult<"/api/costs">> & { error?: string | null }) | null
  >(null);
  const rootId = agent.rootId || agent.id;
  const costScope = JSON.stringify([stateDir, rootId]);
  const [sessionCostState, setSessionCostState] = useState<{
    scope: string;
    value: Json | null;
    updating: boolean;
  }>(() => ({
    scope: costScope,
    value: peekSessionCost(stateDir, rootId),
    updating: true,
  }));
  const sessionCost =
    sessionCostState.scope === costScope
      ? sessionCostState.value
      : peekSessionCost(stateDir, rootId);
  const sessionCostUpdating =
    sessionCostState.scope !== costScope || sessionCostState.updating;
  useEffect(() => {
    let active = true;
    const load = async () => {
      setSessionCostState((previous) =>
        previous.scope === costScope
          ? { ...previous, updating: true }
          : {
              scope: costScope,
              value: peekSessionCost(stateDir, rootId),
              updating: true,
            },
      );
      try {
        const value = await get("/api/session-cost", {
          query: { agent: agent.id },
          timeoutMs: 5000,
        });
        if (!active) return;
        if (value.pricingState === "loading") {
          setSessionCostState((previous) => {
            const cached =
              previous.scope === costScope
                ? previous.value
                : peekSessionCost(stateDir, rootId);
            return {
              scope: costScope,
              value: cached || value,
              updating: Boolean(cached),
            };
          });
        } else {
          const cached = storeSessionCost(stateDir, rootId, value);
          setSessionCostState({
            scope: costScope,
            value: cached || value,
            updating: Boolean(value.refreshing),
          });
        }
      } catch (error) {
        if (active)
          setSessionCostState((previous) => ({
            scope: costScope,
            value:
              previous.scope === costScope
                ? previous.value
                : peekSessionCost(stateDir, rootId),
            updating: false,
          }));
        throw error;
      }
    };
    const stop = watchResourceReads(
      { kind: "session-cost", agentId: agent.id },
      load,
      () => {
        // Keep the cached value while the session cost service is unavailable.
      },
    );
    return () => {
      active = false;
      stop();
    };
  }, [agent.id, costScope, rootId, stateDir]);
  useEffect(() => {
    let active = true;
    const load = async () => {
      try {
        const result = await get("/api/costs", {
          query: { account_key: accountKey },
          timeoutMs: 15000,
        });
        if (!active) return;
        if (result.accountKey !== accountKey)
          throw new Error("Cost account mismatch");
        setCosts(result);
      } catch (error) {
        if (active) {
          setCosts((previous) => ({
            ...previous,
            error: "Local costs unavailable",
          }));
        }
        throw error;
      }
    };
    setCosts(null);
    const stop = watchResourceReads({ kind: "costs" }, load, () => {
      if (active)
        setCosts((previous) => ({
          ...previous,
          error: "Local costs unavailable",
        }));
    });
    return () => {
      active = false;
      stop();
    };
  }, [accountKey]);
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now() / 1000), 30000);
    return () => window.clearInterval(timer);
  }, []);
  const resetCredits = jsonObject(limitsData?.rateLimitResetCredits);
  const resetCount = number(resetCredits?.availableCount)
    ? resetCredits.availableCount
    : null;
  const availableResets: AvailableReset[] = Array.isArray(resetCredits?.credits)
    ? resetCredits.credits.flatMap((value) => {
        const credit = jsonObject(value);
        if (
          !credit ||
          typeof credit.id !== "string" ||
          credit.status !== "available" ||
          (number(credit.expiresAt) && credit.expiresAt <= now)
        )
          return [];
        return [
          {
            id: credit.id,
            ...(typeof credit.title === "string"
              ? { title: credit.title }
              : {}),
            ...(typeof credit.description === "string"
              ? { description: credit.description }
              : {}),
            ...(number(credit.expiresAt)
              ? { expiresAt: credit.expiresAt }
              : {}),
            ...(typeof credit.resetType === "string"
              ? { resetType: credit.resetType }
              : {}),
          },
        ];
      })
    : [];
  const contextUsage = jsonObject(agent.contextUsage);
  const contextTokens = contextUsage?.tokens;
  const contextWindow = contextUsage?.window;
  const known =
    number(contextTokens) && number(contextWindow) && contextWindow > 0;
  const percent = known
    ? Math.round((contextTokens / contextWindow) * 100)
    : null;
  const buckets = readBuckets(limits, now);
  const recovered = useRecoveredLimit(accountAgent, limits, now);
  const recovery = recovered ? null : limitRecovery(accountAgent, limits, now);
  useEffect(() => {
    autoReloadedAccounts.current.clear();
  }, [agent.id]);
  useEffect(() => {
    if (!limitsOpened || !activeAccount || selectedAccountKey === fallbackKey)
      return;
    if (autoReloadedAccounts.current.has(selectedAccountKey)) return;
    autoReloadedAccounts.current.add(selectedAccountKey);
    void activeAccountReload.current?.(false);
  }, [limitsOpened, selectedAccountKey, fallbackKey]);
  const accountIsLow = (item: UsageAccount) =>
    readBuckets(accountLimits(item.limits, item.key, item.accountId), now).some(
      (bucket) =>
        bucket.windows.some(
          (window) =>
            window.remaining !== null &&
            window.remaining <= 15 &&
            !window.expired,
        ),
    );
  const lowAccount = usageAccounts.find(accountIsLow);
  return (
    <div
      className="usage-footer"
      id="usage-footer"
      data-account-key={agent.accountKey || "default"}
    >
      <div
        className="session-cost-summary"
        aria-label="Current team session API cost estimate"
        aria-busy={sessionCostUpdating}
        title={[
          sessionCost
            ? `Session estimate: ${dollars(sessionCost.totalUSD)}`
            : "",
          ...Object.entries(sessionCost?.breakdown?.providers || {}).map(
            ([provider, value]) => `${provider}: ${dollars(value)}`,
          ),
          ...(Array.isArray(sessionCost?.unknownModels) &&
          sessionCost.unknownModels.length
            ? [`Unpriced models: ${sessionCost.unknownModels.join(", ")}`]
            : []),
          ...(sessionCost?.claudeHistoryIncomplete
            ? ["Earlier Claude totals may be incomplete."]
            : []),
          typeof sessionCost?.cacheAgeSeconds === "number"
            ? `Updated ${Math.round(sessionCost.cacheAgeSeconds)} seconds ago`
            : "",
          typeof sessionCost?.method === "string" ? sessionCost.method : "",
        ]
          .filter(Boolean)
          .join("\n")}
      >
        {sessionCost?.pricingState === "loading"
          ? ""
          : sessionCost
            ? `Session estimate: ${dollars(sessionCost.totalUSD)}${
                sessionCost.claudeHistoryIncomplete ||
                sessionCost.unknownModels?.length
                  ? "*"
                  : ""
              }`
            : ""}
      </div>
      <TokenRate agent={agent} />
      <Popover
        opened={contextOpen}
        onChange={setContextOpen}
        position="top-start"
        width="min(340px, calc(100vw - 24px))"
        trapFocus
        returnFocus
      >
        <Popover.Target>
          <button
            type="button"
            className="context-use"
            aria-label="Chat context"
            aria-expanded={contextOpen}
            onClick={() => setContextOpen(!contextOpen)}
          >
            {percent === null ? "Context" : `Context ${percent}%`}
          </button>
        </Popover.Target>
        <Popover.Dropdown>
          <strong>Chat context</strong>
          <p>
            {known
              ? `${contextTokens.toLocaleString()} of ${contextWindow.toLocaleString()} tokens used (${percent}%).`
              : "No context data yet."}
          </p>
          {agent.compactions !== undefined && (
            <small>Compacted {agent.compactions} times.</small>
          )}
          <Button
            variant="subtle"
            onClick={() => {
              setContextOpen(false);
              setAnalyticsOpen(true);
            }}
          >
            Advanced analytics
          </Button>
        </Popover.Dropdown>
      </Popover>
      {analyticsOpen && (
        <Suspense fallback={null}>
          <Analytics
            key={agent.id}
            agent={agent}
            onClose={() => setAnalyticsOpen(false)}
          />
        </Suspense>
      )}
      <div className="account-limits-group">
        <div
          className="account-limits-dots"
          role="group"
          aria-label="Account weekly allowance"
        >
          {usageAccounts.map((item) => {
            const projection = weeklyRunway(
              accountLimits(item.limits, item.key, item.accountId),
              now,
              item.signedOut,
            );
            const current = item.key === fallbackKey;
            const provider =
              item.provider === "claude"
                ? "Claude"
                : item.provider &&
                    item.provider !== "codex" &&
                    item.provider !== "openai"
                  ? item.provider
                  : "Codex";
            const label = humanAccountLabel(item);
            const details = [
              `${label} (${provider})${current ? ", current chat account" : ""}`,
              ...(item.signedOut
                ? ["Signed out. Weekly allowance unavailable."]
                : []),
              ...(!item.signedOut &&
              typeof item.limits?.error === "string" &&
              !item.limits?.data
                ? [`Limits unavailable: ${item.limits.error}`]
                : []),
              ...projection.windows.map(
                (window) =>
                  `${window.name} weekly: ${window.remaining === null ? "remaining allowance unavailable" : `${formatPercent(window.remaining)} left`}. ${window.reset === null ? "Reset time unavailable." : window.reset <= now ? "Cached reset passed." : `Resets ${localDateTime(new Date(window.reset * 1000), { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}.`}${window.days !== null && Number.isFinite(window.days) ? ` About ${window.days.toFixed(1)} days left at the current rate.` : ""}${window.resetFirst ? " Resets before this allowance runs out." : ""}`,
              ),
              projection.days === null
                ? "Projected days unavailable."
                : !Number.isFinite(projection.days)
                  ? "At least 7 days left at the current rate. No weekly consumption recorded."
                  : projection.windows.every((window) => window.resetFirst)
                    ? "At least 7 days left at the current rate after reset."
                    : projection.windows.length > 1
                      ? `About ${projection.days.toFixed(1)} days left across weekly limits.`
                      : "",
              "Rate uses consumption in the elapsed part of the weekly window.",
            ]
              .filter(Boolean)
              .join("\n");
            return (
              <Tooltip
                key={item.key}
                label={
                  <span className="account-limits-dot-tooltip">{details}</span>
                }
                multiline
                w={300}
                withArrow
                events={{ hover: true, focus: true, touch: true }}
              >
                <button
                  type="button"
                  className="account-limits-dot-target"
                  data-account-key={item.key}
                  aria-label={`Open ${humanAccountLabel(item)} weekly allowance (${provider}). ${details}`}
                  aria-current={current ? "true" : undefined}
                  aria-haspopup="dialog"
                  aria-expanded={
                    limitsOpened && selectedAccountKey === item.key
                  }
                  onClick={() => {
                    setActiveAccountKey(item.key);
                    changeOpened(true);
                  }}
                >
                  <span
                    className="account-limits-dot"
                    data-color={projection.color}
                    data-current={current || undefined}
                    aria-hidden="true"
                  />
                </button>
              </Tooltip>
            );
          })}
        </div>
        <Popover
          opened={limitsOpened}
          onChange={changeOpened}
          position="top-end"
          width="min(370px, calc(100vw - 24px))"
          withArrow
          shadow="lg"
          trapFocus
          returnFocus
        >
          <Popover.Target>
            <Button
              className="limits-toggle account-limits-toggle"
              variant="subtle"
              size="compact-xs"
              aria-label="Account limits"
              title={
                lowAccount
                  ? `Low allowance: ${lowAccount.email || lowAccount.label}`
                  : undefined
              }
              onClick={() => changeOpened(!limitsOpened)}
              leftSection={<Gauge size={13} />}
              rightSection={<ChevronUp size={12} />}
            >
              <span className="account-limits-summary">
                <span className="account-limits-label">Limits</span>
                {lowAccount && (
                  <span className="account-limits-warning">Low allowance</span>
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
                </div>
                <Button
                  size="compact-xs"
                  variant="subtle"
                  loading={refreshing || loadingLimits}
                  onClick={() =>
                    void (activeAccount?.reload(true) ?? refreshLimits())
                  }
                  leftSection={<RefreshCw size={13} />}
                >
                  Refresh
                </Button>
              </header>
              <AccountTilesLayout
                label="Accounts with limits"
                provider={accountProvider(activeAccount)}
                providers={[...new Set(usageAccounts.map(accountProvider))]}
                onProvider={(provider) => {
                  const account = usageAccounts.find(
                    (item) => accountProvider(item) === provider,
                  );
                  if (account) setActiveAccountKey(account.key);
                }}
              >
                <Tabs
                  value={selectedAccountKey}
                  onChange={(value) => {
                    if (value) setActiveAccountKey(value);
                  }}
                  keepMounted={false}
                  variant="pills"
                  className="account-limits-tiles"
                >
                  <Tabs.List aria-label="Accounts with limits">
                    {usageAccounts.map((item, index) => {
                      if (
                        accountProvider(item) !== accountProvider(activeAccount)
                      )
                        return null;
                      const snapshot = accountLimits(
                        item.limits,
                        item.key,
                        item.accountId,
                      );
                      const remaining = remainingLimit(
                        readBuckets(snapshot, now).flatMap(
                          (bucket) => bucket.windows,
                        ),
                      );
                      return (
                        <Tabs.Tab
                          key={item.key}
                          value={item.key}
                          className="setup-account"
                          data-account-key={item.key}
                          data-selected={
                            item.key === selectedAccountKey || undefined
                          }
                          aria-current={
                            item.key === fallbackKey ? "true" : undefined
                          }
                          id={`usage-account-tab-${index}`}
                          aria-controls="usage-account-panel"
                          title={humanAccountLabel(item)}
                          aria-label={`${humanAccountLabel(item)}${item.provider ? `, ${item.provider}` : ""} account limits`}
                        >
                          <span className="account-limits-tile-name">
                            {displayAccountName(item)}
                          </span>
                          {item.key === fallbackKey && (
                            <small className="account-limits-current">
                              Current chat
                            </small>
                          )}
                          <AccountRemainingBar
                            remaining={remaining}
                            label={`${humanAccountLabel(item)} remaining limit`}
                          />
                        </Tabs.Tab>
                      );
                    })}
                  </Tabs.List>
                </Tabs>
              </AccountTilesLayout>
              <div className="account-limits-selected-name">
                <strong>{displayAccountName(activeAccount)}</strong>
                {!activeAccount?.email &&
                  (!activeAccount?.label ||
                    activeAccount.label === activeAccount.key) && (
                    <small>{activeAccount?.key || fallbackKey}</small>
                  )}
              </div>
              {recovery && (
                <LimitRecoveryNotice
                  key={JSON.stringify(recovery)}
                  recovery={recovery}
                />
              )}
              <div
                className="account-limits-groups"
                role="tabpanel"
                id="usage-account-panel"
                aria-labelledby={`usage-account-tab-${usageAccounts.findIndex((item) => item.key === selectedAccountKey)}`}
                aria-label={`${humanAccountLabel(activeAccount)} limits`}
              >
                {buckets.map((bucket) => (
                  <section
                    className="account-limit-group"
                    key={bucket.id}
                    aria-label={`${bucket.name} limits`}
                  >
                    <header>
                      <strong>{bucket.name}</strong>
                      {typeof bucket.data.planType === "string" && (
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
                              size={3}
                              radius="xl"
                              color={
                                window.remaining > 50
                                  ? "green"
                                  : window.remaining >= 15
                                    ? "yellow"
                                    : "red"
                              }
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
                                  title={new Date(
                                    window.reset * 1000,
                                  ).toString()}
                                >
                                  {localDateTime(
                                    new Date(window.reset * 1000),
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
                    {jsonObject(bucket.data.credits) && (
                      <div className="account-limit-credits">
                        <span>Credits</span>
                        <strong>
                          {jsonObject(bucket.data.credits)?.unlimited === true
                            ? "Unlimited"
                            : jsonObject(bucket.data.credits)?.balance != null
                              ? String(jsonObject(bucket.data.credits)?.balance)
                              : jsonObject(bucket.data.credits)?.hasCredits ===
                                  true
                                ? "Available"
                                : jsonObject(bucket.data.credits)
                                      ?.hasCredits === false
                                  ? "None"
                                  : "Unavailable"}
                        </strong>
                      </div>
                    )}
                  </section>
                ))}
                {!buckets.length && (
                  <p className="account-limits-empty">
                    {loadingLimits
                      ? "Loading account limits…"
                      : "Codex has not supplied account limits."}
                  </p>
                )}
              </div>
              {activeAccount?.provider === "claude" && (
                <section
                  className="account-reset-credits"
                  aria-label="Claude free limit resets"
                >
                  <header>
                    <strong>Free limit resets</strong>
                    <span>Check on Claude</span>
                  </header>
                  <p>
                    Opens Claude. Use the same account, then confirm “Reset for
                    free”.
                  </p>
                  <div className="account-reset-credit-row">
                    <Button
                      component="a"
                      href="https://claude.ai/settings/usage"
                      target="_blank"
                      rel="noopener noreferrer"
                      size="compact-xs"
                      variant="default"
                      rightSection={<ExternalLink size={13} />}
                    >
                      Open free resets
                    </Button>
                    <Button
                      size="compact-xs"
                      variant="subtle"
                      loading={refreshing || loadingLimits}
                      onClick={() => void refreshLimits()}
                    >
                      Refresh after reset
                    </Button>
                  </div>
                </section>
              )}
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
                                  {localDateTime(
                                    new Date(credit.expiresAt * 1000),
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
                              "Expiration unavailable"
                            )}
                          </span>
                        </div>
                        <Button
                          size="compact-xs"
                          variant="light"
                          disabled={
                            resetPending ||
                            resetApplied.includes(resetAppliedKey(credit.id)) ||
                            credit.resetType !== "codexRateLimits" ||
                            typeof limitsData?.accountId !== "string"
                          }
                          onClick={() => {
                            setConfirmReset(credit.id);
                            setResetError("");
                          }}
                        >
                          {resetApplied.includes(resetAppliedKey(credit.id))
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
                  <strong>Estimated API cost</strong>
                  <span>USD</span>
                </header>
                <div className="account-cost-values">
                  {(
                    [
                      ["Today", costs?.data?.todayUSD],
                      ["Last 30 days", costs?.data?.last30DaysUSD],
                    ] as const
                  ).map(([label, value]) => (
                    <div key={label}>
                      <span>{label}</span>
                      <strong
                        className={
                          number(value) ? undefined : "account-cost-missing"
                        }
                      >
                        {dollars(value)}
                      </strong>
                    </div>
                  ))}
                </div>
                {(costs?.data?.coverage === "partial" ||
                  costs?.stale ||
                  costs?.error) && (
                  <p className="account-cost-caption">
                    {costs?.error || costs?.stale
                      ? number(costs?.data?.todayUSD) ||
                        number(costs?.data?.last30DaysUSD)
                        ? "Saved estimate"
                        : "Estimate unavailable"
                      : "Partial estimate"}
                  </p>
                )}
                {typeof costs?.data?.note === "string" && (
                  <p className="account-cost-caption">{costs.data.note}</p>
                )}
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
                  ` · ${localTime(new Date(limits!.at * 1000), { hour: "2-digit", minute: "2-digit" })}`}
              </footer>
            </section>
          </Popover.Dropdown>
        </Popover>
      </div>
    </div>
  );
}
