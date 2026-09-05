import { Button, Popover, Progress, Tooltip } from "@mantine/core";
import { ChevronUp, Gauge, RotateCcw } from "lucide-react";
import type { Agent, Json } from "../types";
export default function Usage({
  agent,
  limits,
  reload,
}: {
  agent: Agent;
  limits: Json | null;
  reload: () => void;
}) {
  const c = agent.contextUsage,
    known =
      c &&
      typeof c.tokens === "number" &&
      typeof c.window === "number" &&
      c.window > 0;
  const percent = known ? Math.round((c.tokens! / c.window!) * 100) : null;
  const buckets =
    limits?.data?.rateLimitsByLimitId ||
    (limits?.data?.rateLimits ? { codex: limits.data.rateLimits } : {});
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
      <Popover position="top-end" width={300} withArrow shadow="lg" trapFocus>
        <Popover.Target>
          <Button
            className="limits-toggle"
            size="compact-xs"
            leftSection={<Gauge size={13} />}
            rightSection={<ChevronUp size={12} />}
          >
            Limits
          </Button>
        </Popover.Target>
        <Popover.Dropdown>
          <div className="limits-card">
            <strong>Account limits</strong>
            {Object.entries(buckets).map(([key, value]) => {
              const b = value as Json;
              return (
                <div key={key}>
                  <p>
                    {b.limitName || key}
                    {b.planType ? ` · ${b.planType}` : ""}
                  </p>
                  {["primary", "secondary"].map(
                    (w) =>
                      b[w] && (
                        <p key={w}>
                          {b[w].windowDurationMins
                            ? `${b[w].windowDurationMins / 60}h`
                            : w}
                          : {b[w].usedPercent}% used
                          {b[w].resetsAt && (
                            <small>
                              Resets{" "}
                              {new Date(b[w].resetsAt * 1000).toLocaleString()}
                            </small>
                          )}
                        </p>
                      ),
                  )}
                  {b.credits && (
                    <p>
                      {b.credits.unlimited
                        ? "Unlimited credits"
                        : b.credits.balance != null
                          ? `Credits: ${b.credits.balance}`
                          : b.credits.hasCredits
                            ? "Credits available"
                            : "No credits"}
                    </p>
                  )}
                  {b.individualLimit && (
                    <p>
                      Individual: {b.individualLimit.remainingPercent}% left
                    </p>
                  )}
                </div>
              );
            })}
            {!Object.keys(buckets).length && (
              <p>Codex has not supplied account limits.</p>
            )}
            {limits?.error && <p className="danger">{limits.error}</p>}
            {limits?.at && (
              <small>
                Updated {new Date(limits.at * 1000).toLocaleTimeString()}
              </small>
            )}
            <Button onClick={reload} variant="light" fullWidth>
              Refresh limits
            </Button>
          </div>
        </Popover.Dropdown>
      </Popover>
    </div>
  );
}
