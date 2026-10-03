import type { Json } from "./types";

const DAY = 86400;
const WEEK_MINUTES = 10080;
const finite = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value);

export type WeeklyRunway = {
  color: "green" | "yellow" | "red" | "gray";
  days: number | null;
  windows: {
    name: string;
    remaining: number | null;
    reset: number | null;
    days: number | null;
    resetFirst: boolean;
  }[];
};

// The cached observation supplies consumption and its time. No provider read is needed.
export function weeklyRunway(
  limits: Json | null,
  now: number,
  signedOut = false,
): WeeklyRunway {
  const reported = limits?.data?.rateLimitsByLimitId;
  const buckets: [string, Json][] = Object.entries(
    reported && Object.keys(reported).length
      ? reported
      : limits?.data?.rateLimits
        ? {
            [limits.data.rateLimits.limitId || "codex"]: limits.data.rateLimits,
          }
        : {},
  );
  const windows = buckets.flatMap(([id, bucket]) =>
    [bucket?.primary, bucket?.secondary].flatMap((window) => {
      if (window?.windowDurationMins !== WEEK_MINUTES) return [];
      const used =
        finite(window.usedPercent) &&
        window.usedPercent >= 0 &&
        window.usedPercent <= 100
          ? window.usedPercent
          : null;
      const remaining = used === null ? null : 100 - used;
      const reset =
        finite(window.resetsAt) && window.resetsAt > 0 ? window.resetsAt : null;
      const observed = limits?.at;
      const start = reset === null ? null : reset - WEEK_MINUTES * 60;
      const elapsed =
        finite(observed) && start !== null ? (observed - start) / DAY : null;
      const valid =
        !signedOut &&
        reset !== null &&
        reset > now &&
        finite(observed) &&
        observed <= now + 5 &&
        elapsed !== null &&
        elapsed > 0 &&
        elapsed <= 7;
      const days =
        valid && remaining !== null && used !== null
          ? used === 0
            ? Infinity
            : remaining / (used / elapsed!)
          : null;
      return [
        {
          name:
            bucket.limitName ||
            (id === "codex" ? "Codex" : id === "claude" ? "Claude" : id),
          remaining,
          reset,
          days,
          resetFirst:
            days !== null &&
            days > 0 &&
            reset !== null &&
            (reset - now) / DAY < days,
        },
      ];
    }),
  );
  if (signedOut || !windows.length)
    return { color: "gray", days: null, windows };
  if (
    windows.some(
      (window) =>
        window.remaining === 0 && (window.reset === null || window.reset > now),
    )
  )
    return { color: "red", days: 0, windows };
  if (windows.some((window) => window.days === null))
    return { color: "gray", days: null, windows };
  const days = Math.min(
    ...windows.map((window) =>
      window.resetFirst ? Math.max(7, window.days!) : window.days!,
    ),
  );
  return {
    color: days >= 7 ? "green" : days >= 5 ? "yellow" : "red",
    days,
    windows,
  };
}
