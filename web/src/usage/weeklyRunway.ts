import type { Json } from "../types";

const DAY = 86400;
const WEEK_MINUTES = 10080;
const finite = (value: unknown): value is number => Number.isFinite(value);

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
      const reset =
        finite(window.resetsAt) && window.resetsAt > 0 ? window.resetsAt : null;
      const expired = reset !== null && reset <= now;
      const remaining = expired ? 100 : used === null ? null : 100 - used;
      const observed = limits!.at;
      const observedAt = finite(observed) ? observed : Number.NaN;
      const elapsed =
        reset !== null
          ? (observedAt - (reset - WEEK_MINUTES * 60)) / DAY
          : null;
      const elapsedDays = elapsed ?? Number.NaN;
      const valid =
        !signedOut &&
        !expired &&
        observedAt <= now + 5 &&
        elapsedDays > 0 &&
        elapsedDays <= 7;
      const days =
        expired || used === 0
          ? Infinity
          : valid && used !== null
            ? (100 - used) / (used / elapsedDays)
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
            reset !== null && (reset - now) / DAY < (days ?? Number.NaN),
        },
      ];
    }),
  );
  if (
    signedOut ||
    !limits?.data ||
    (windows.length > 0 && windows.every((window) => window.remaining === null))
  )
    return { color: "gray", days: null, windows };
  if (windows.some((window) => window.remaining === 0))
    return { color: "red", days: 0, windows };
  if (windows.some((window) => window.days === null))
    return { color: "red", days: null, windows };
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
