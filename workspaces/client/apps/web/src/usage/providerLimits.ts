import type { components } from "../generated/api";
import type { JsonInput, JsonInputOf } from "../types";
import { jsonObject } from "./accountUsage";

type UsageLimitsData = JsonInputOf<components["schemas"]["UsageLimitsData"]>;
type RateLimitsDataDto = JsonInputOf<
  components["schemas"]["RateLimitsDataDto"]
>;
type RateLimitBucketDto = JsonInputOf<
  components["schemas"]["RateLimitBucketDto"]
>;
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

export function providerRateLimitBucket(
  value: JsonInput | null | undefined,
): RateLimitBucket | null {
  if (value == null) return null;
  return isRateLimitBucket(value) ? value : null;
}

function isRateLimitBucket(
  value: JsonInput | undefined,
): value is RateLimitBucket {
  const bucket = jsonObject(value);
  return Boolean(
    bucket &&
    optional(bucket.limitId, nullableString) &&
    optional(bucket.limitName, nullableString) &&
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

function isRateLimitBucketDto(
  value: JsonInput | undefined,
): value is RateLimitBucketDto {
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

function isRateLimitBucketDtoMap(value: JsonInput | undefined): boolean {
  const buckets = jsonObject(value);
  return Boolean(buckets && Object.values(buckets).every(isRateLimitBucketDto));
}

export function providerRateLimitsDataDto(
  value: JsonInput | null | undefined,
): RateLimitsDataDto | null {
  const data = value == null ? null : jsonObject(value);
  return data &&
    optional(data.accountId, nullableString) &&
    optional(data.ordinaryUsageAllowed, nullableBoolean) &&
    optional(
      data.rateLimits,
      (bucket) => bucket === null || isRateLimitBucketDto(bucket),
    ) &&
    optional(
      data.rateLimitsByLimitId,
      (buckets) => buckets === null || isRateLimitBucketDtoMap(buckets),
    ) &&
    optional(data.signedIn, nullableBoolean) &&
    optional(data.status, nullableString)
    ? (data as RateLimitsDataDto)
    : null;
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

export function providerLimitData(
  value: JsonInput | null | undefined,
): UsageLimitsData | null {
  if (value == null) return null;
  if (isUsageLimitsData(value)) return value;
  const dto = providerRateLimitsDataDto(value);
  if (!dto) return null;
  const bucket = (value: RateLimitBucketDto): RateLimitBucket => {
    const raw = jsonObject(value);
    const normalized: RateLimitBucket = { ...value };
    if (raw && raw.spendControlReached !== undefined)
      normalized.spendControlReached = isBoolean(raw.spendControlReached)
        ? raw.spendControlReached
        : null;
    if (raw && raw.individualLimit !== undefined) {
      const individual = raw.individualLimit;
      normalized.individualLimit =
        individual === null || isRateLimitIndividualLimit(individual)
          ? individual
          : null;
    }
    return normalized;
  };
  const credits = dto.rateLimitResetCredits;
  let normalizedCredits: RateLimitResetCredits | null | undefined;
  if (credits == null) normalizedCredits = credits;
  else if (isRateLimitResetCredits(credits)) normalizedCredits = credits;
  else return null;
  return {
    ...(dto.accountId !== undefined ? { accountId: dto.accountId } : {}),
    ...(dto.ordinaryUsageAllowed !== undefined
      ? { ordinaryUsageAllowed: dto.ordinaryUsageAllowed }
      : {}),
    ...(dto.signedIn !== undefined ? { signedIn: dto.signedIn } : {}),
    ...(dto.status !== undefined ? { status: dto.status } : {}),
    ...(normalizedCredits !== undefined
      ? { rateLimitResetCredits: normalizedCredits }
      : {}),
    ...(dto.rateLimits !== undefined
      ? {
          rateLimits: dto.rateLimits === null ? null : bucket(dto.rateLimits),
        }
      : {}),
    ...(dto.rateLimitsByLimitId !== undefined
      ? {
          rateLimitsByLimitId:
            dto.rateLimitsByLimitId === null
              ? null
              : Object.fromEntries(
                  Object.entries(dto.rateLimitsByLimitId).map(([id, item]) => [
                    id,
                    bucket(item),
                  ]),
                ),
        }
      : {}),
  };
}
