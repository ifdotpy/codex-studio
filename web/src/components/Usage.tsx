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
      <span className="context-use">
        <meter
          min={0}
          max={100}
          value={percent || 0}
          aria-label="Context used"
        />
        {percent === null
          ? "Context unavailable"
          : `Context ${percent}% · ${c!.tokens!.toLocaleString()} / ${c!.window!.toLocaleString()}`}
      </span>
      <span>
        {agent.compactions === undefined
          ? "Compactions unavailable"
          : `${agent.compactions} ${agent.compactionsObservedOnly ? "observed " : ""}compactions`}
      </span>
      <details className="limits">
        <summary>Limits</summary>
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
                  <p>Individual: {b.individualLimit.remainingPercent}% left</p>
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
          <button onClick={reload}>Refresh limits</button>
        </div>
      </details>
      {known && <small className="context-note">Last reported context</small>}
    </div>
  );
}
