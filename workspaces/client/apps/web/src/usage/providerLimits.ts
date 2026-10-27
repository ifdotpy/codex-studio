import type { components } from "../generated/api";
import type { JsonInput, JsonInputOf } from "../types";
import { jsonObject } from "./accountUsage";

type UsageLimitsData = JsonInputOf<components["schemas"]["UsageLimitsData"]>;
export type RateLimitBucket = JsonInputOf<
  components["schemas"]["RateLimitBucket"]
>;
export type RateLimitWindow = JsonInputOf<
  components["schemas"]["RateLimitWindow"]
>;
type RateLimitIndividualLimit = JsonInputOf<
  components["schemas"]["RateLimitIndividualLimit"]
>;
type RateLimitResetCredits = JsonInputOf<
  components["schemas"]["RateLimitResetCredits"]
>;
type RateLimitResetCredit = JsonInputOf<
  components["schemas"]["RateLimitResetCredit"]
>;

const isFiniteNumber = (value: JsonInput | undefined): value is number =>
  typeof value === "number" && Number.isFinite(value);
const isString = (value: JsonInput | undefined): value is string =>
  typeof value === "string";
const isBoolean = (value: JsonInput | undefined): value is boolean =>
  typeof value === "boolean";
const optional = (
  value: JsonInput | undefined,
  valid: (value: JsonInput | undefined) => boolean,
) => value === undefined || valid(value);
const nullableString = (value: JsonInput | undefined) =>
  value === null || isString(value);
const nullableBoolean = (value: JsonInput | undefined) =>
  value === null || isBoolean(value);
const nullableNumber = (value: JsonInput | undefined) =>
  value === null || isFiniteNumber(value);

function isRateLimitWindow(
  value: JsonInput | undefined,
): value is RateLimitWindow {
  const window = jsonObject(value);
  return Boolean(
    window &&
    optional(window.usedPercent, nullableNumber) &&
    optional(window.resetsAt, nullableNumber) &&
    optional(window.windowDurationMins, nullableNumber),
  );
}

function isRateLimitIndividualLimit(
  value: JsonInput | undefined,
): value is RateLimitIndividualLimit {
  const limit = jsonObject(value);
  return Boolean(limit && optional(limit.remainingPercent, nullableNumber));
}

function isRateLimitResetCredit(
  value: JsonInput | undefined,
): value is RateLimitResetCredit {
  const credit = jsonObject(value);
  return Boolean(
    credit &&
    typeof credit.id === "string" &&
    typeof credit.resetType === "string" &&
    typeof credit.status === "string" &&
    optional(credit.grantedAt, nullableNumber) &&
    optional(credit.expiresAt, nullableNumber),
  );
}

function isRateLimitResetCredits(
  value: JsonInput | undefined,
): value is RateLimitResetCredits {
  const credits = jsonObject(value);
  return Boolean(
    credits &&
    optional(credits.availableCount, nullableNumber) &&
    optional(
      credits.credits,
      (items) => Array.isArray(items) && items.every(isRateLimitResetCredit),
    ),
  );
}

function isRateLimitBucket(
  value: JsonInput | undefined,
): value is RateLimitBucket {
  const bucket = jsonObject(value);
  return Boolean(
    bucket &&
    optional(bucket.limitId, nullableString) &&
    optional(bucket.limitName, nullableString) &&
    optional(bucket.planType, nullableString) &&
    optional(bucket.rateLimitReachedType, nullableString) &&
    optional(bucket.spendControlReached, nullableBoolean) &&
    optional(
      bucket.individualLimit,
      (limit) => limit === null || isRateLimitIndividualLimit(limit),
    ) &&
    optional(
      bucket.primary,
      (window) => window === null || isRateLimitWindow(window),
    ) &&
    optional(
      bucket.secondary,
      (window) => window === null || isRateLimitWindow(window),
    ),
  );
}

function isRateLimitBucketMap(value: JsonInput | undefined): boolean {
  const buckets = jsonObject(value);
  return Boolean(buckets && Object.values(buckets).every(isRateLimitBucket));
}

function isUsageLimitsData(
  value: JsonInput | undefined,
): value is UsageLimitsData {
  const data = jsonObject(value);
  if (
    !data ||
    !optional(data.accountId, nullableString) ||
    !optional(data.ordinaryUsageAllowed, nullableBoolean) ||
    !optional(
      data.rateLimitResetCredits,
      (credits) => credits === null || isRateLimitResetCredits(credits),
    ) ||
    !optional(
      data.rateLimits,
      (bucket) => bucket === null || isRateLimitBucket(bucket),
    ) ||
    !optional(
      data.rateLimitsByLimitId,
      (buckets) => buckets === null || isRateLimitBucketMap(buckets),
    ) ||
    !optional(data.signedIn, nullableBoolean) ||
    !optional(data.source, nullableString) ||
    !optional(data.status, nullableString)
  )
    return false;
  return true;
}

// Only durable local-storage hydration uses these checks. API/SSE consumers use
// the generated contract checked by the server's LimitsSnapshot ingress.
export function storedLimitData(value: JsonInput): UsageLimitsData | null {
  return isUsageLimitsData(value) ? value : null;
}
