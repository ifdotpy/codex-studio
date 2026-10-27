import type { SDKRateLimitInfo } from "@anthropic-ai/claude-agent-sdk";
import type { UnsupportedClaudeRateLimit } from "./runtime-contract.generated.ts";
import { z } from "zod";
import usagePolicyJson from "./usage-policy.ts";

export const usagePolicy: {
  modelBuckets: Record<string, string>;
  allowedOverageStatuses: string[];
} = JSON.parse(usagePolicyJson);
export type RateLimitInfo = Pick<
  SDKRateLimitInfo,
  | "status"
  | "rateLimitType"
  | "resetsAt"
  | "utilization"
  | "overageStatus"
  | "overageResetsAt"
  | "isUsingOverage"
  | "overageInUse"
>;
// Window duration owner, in minutes; SDK utilization is fractional on events.
export const rateWindowMinutes = { primary: 300, secondary: 10080 } as const;
const scopes = {
  five_hour: {
    native: "five_hour",
    id: "claude",
    label: "Claude",
    window: "primary",
    family: null,
  },
  seven_day: {
    native: "seven_day",
    id: "claude",
    label: "Claude",
    window: "secondary",
    family: null,
  },
  seven_day_opus: {
    native: "seven_day_opus",
    id: usagePolicy.modelBuckets.opus,
    label: "Claude · opus",
    window: "secondary",
    family: "opus",
  },
  seven_day_sonnet: {
    native: "seven_day_sonnet",
    id: usagePolicy.modelBuckets.sonnet,
    label: "Claude · sonnet",
    window: "secondary",
    family: "sonnet",
  },
  // This event does not identify its dynamic model bucket. Preserve evidence;
  // do not infer a model from whichever cached bucket happens to be present.
  seven_day_overage_included: {
    native: "seven_day_overage_included",
    id: null,
    label: null,
    window: "secondary",
    family: null,
  },
  overage: {
    native: "overage",
    id: "claude-overage",
    label: "Claude · overage",
    window: "secondary",
    family: null,
  },
} satisfies {
  [K in NonNullable<SDKRateLimitInfo["rateLimitType"]>]: {
    native: K;
    id: string | null | undefined;
    label: string | null;
    window: keyof typeof rateWindowMinutes;
    family: string | null;
  };
};
const statuses = {
  allowed: { native: "allowed", rejected: false },
  allowed_warning: { native: "allowed_warning", rejected: false },
  rejected: { native: "rejected", rejected: true },
} satisfies {
  [K in SDKRateLimitInfo["status"]]: { native: K; rejected: boolean };
};
const overageStatuses = {
  allowed: {
    native: "allowed",
    allowed: usagePolicy.allowedOverageStatuses.includes("allowed"),
  },
  allowed_warning: {
    native: "allowed_warning",
    allowed: usagePolicy.allowedOverageStatuses.includes("allowed_warning"),
  },
  rejected: { native: "rejected", allowed: false },
} satisfies {
  [K in NonNullable<SDKRateLimitInfo["overageStatus"]>]: {
    native: K;
    allowed: boolean;
  };
};

// This SDK-backed projection is also rendered as JSON Schema for Rust readers.
// Other SDK fields stay in the opaque original payload and are not interpreted.
export const rateLimitSchema = z.looseObject({
  status: z.enum(Object.keys(statuses)),
  rateLimitType: z.enum(Object.keys(scopes)).optional(),
  overageStatus: z.enum(Object.keys(overageStatuses)).optional(),
  utilization: z.number().finite().optional(),
  resetsAt: z.number().finite().optional(),
  overageResetsAt: z.number().finite().optional(),
  isUsingOverage: z.boolean().optional(),
  overageInUse: z.boolean().optional(),
});
export function parseRateLimitInfo(raw: unknown) {
  const unsupported = {
    type: "unsupported",
    raw,
  } satisfies UnsupportedClaudeRateLimit;
  const parsed = rateLimitSchema.safeParse(raw);
  if (!parsed.success) return unsupported;
  const data = parsed.data;
  const scope = Object.values(scopes).find(
    (value) => value.native === data.rateLimitType,
  );
  const status = Object.values(statuses).find(
    (value) => value.native === data.status,
  );
  const overage = Object.values(overageStatuses).find(
    (value) => value.native === data.overageStatus,
  );
  if (!scope || scope.id == null || !status) return unsupported;
  const info: RateLimitInfo = {
    status: status.native,
    rateLimitType: scope.native,
    ...(overage === undefined ? {} : { overageStatus: overage.native }),
    ...(data.utilization === undefined
      ? {}
      : { utilization: data.utilization }),
    ...(data.resetsAt === undefined ? {} : { resetsAt: data.resetsAt }),
    ...(data.overageResetsAt === undefined
      ? {}
      : { overageResetsAt: data.overageResetsAt }),
    ...(data.isUsingOverage === undefined
      ? {}
      : { isUsingOverage: data.isUsingOverage }),
    ...(data.overageInUse === undefined
      ? {}
      : { overageInUse: data.overageInUse }),
  };
  return {
    type: "known",
    info,
    raw,
    scope: { ...scope, id: scope.id },
  } as const;
}
export function rateLimitRejected(info: RateLimitInfo): boolean {
  return statuses[info.status].rejected;
}
export function overageAllowed(info: RateLimitInfo): boolean {
  if (
    info.overageStatus !== undefined &&
    !overageStatuses[info.overageStatus].allowed
  )
    return false;
  return (
    (info.overageStatus !== undefined &&
      overageStatuses[info.overageStatus].allowed) ||
    info.isUsingOverage === true ||
    info.overageInUse === true
  );
}
export function rateLimitFamily(info: RateLimitInfo): string | null {
  return info.rateLimitType === undefined
    ? null
    : scopes[info.rateLimitType].family;
}
export function rateLimitJsonSchema() {
  return z.toJSONSchema(rateLimitSchema, { io: "input" });
}
