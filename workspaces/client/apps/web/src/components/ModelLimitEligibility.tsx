import { useEffect, useState } from "react";
import type { GetResult } from "../api";
import { localDateTime } from "../local-time";

export type UsageEligibility = NonNullable<
  GetResult<"/api/limits">["eligibility"]
>;

export default function ModelLimitEligibility({
  eligibility,
  now,
}: {
  eligibility?: UsageEligibility | null;
  now: number;
}) {
  const [clock, setClock] = useState(now);
  useEffect(() => {
    setClock(now);
    if (eligibility?.validUntil == null) return;
    const timer = setTimeout(
      () => setClock(Date.now() / 1000),
      Math.max(0, eligibility.validUntil * 1000 - Date.now() + 1),
    );
    return () => clearTimeout(timer);
  }, [eligibility?.validUntil, now]);
  if (!eligibility)
    return <p>Model eligibility is unavailable. Refresh account limits.</p>;
  const current =
    eligibility.fresh &&
    eligibility.validUntil != null &&
    Math.max(now, clock) < eligibility.validUntil;
  const labels = {
    allowed: "Overage allowed",
    in_use: "Overage in use",
    rejected: "Overage rejected",
    unavailable: "Overage permission unavailable",
  };
  return (
    <section className="model-limit-eligibility" aria-label="Model eligibility">
      <strong>
        {eligibility.model || "Selected model"}:{" "}
        {current
          ? eligibility.canRun
            ? "Can run now"
            : "Cannot run now"
          : "Current eligibility is unknown"}
      </strong>
      {!current && <p>Refresh account limits for a current decision.</p>}
      <p>
        Applicable buckets:{" "}
        {[
          ...new Set(
            eligibility.buckets.map((bucket) => bucket.id || "default"),
          ),
        ].join(", ") || "Unavailable"}
      </p>
      {eligibility.provider === "claude" && (
        <p>{labels[eligibility.overage]}</p>
      )}
      {eligibility.resetAt != null && (
        <p>
          Reported reset:{" "}
          <time dateTime={new Date(eligibility.resetAt * 1000).toISOString()}>
            {localDateTime(new Date(eligibility.resetAt * 1000))}
          </time>
        </p>
      )}
    </section>
  );
}
