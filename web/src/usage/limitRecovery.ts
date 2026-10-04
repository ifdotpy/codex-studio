import type { Agent, Json } from "../types";
import { nativeErrorKind } from "../nativeErrors";
import { providerLimitData, type RateLimitWindow } from "./providerLimits";

export type LimitRecovery = {
  title: string;
  message: string;
  resetAt?: number;
  action?: { label: string; href: string };
  ownerRequest?: string;
};
const finite = (value: unknown): value is number => Number.isFinite(value);

function freshLimits(limits: Json | null, agent: Agent, now: number) {
  return Boolean(
    limits &&
    !limits.error &&
    !limits.stale &&
    !limits.loading &&
    (limits.accountKey || "default") === (agent.accountKey || "default") &&
    finite(limits.at) &&
    now - limits.at <= 300,
  );
}

function windowAllowsUsage(
  window: RateLimitWindow | undefined | null,
  now: number,
): boolean {
  if (window == null) return true;
  const { usedPercent, resetsAt } = window;
  return (
    finite(usedPercent) &&
    usedPercent >= 0 &&
    usedPercent < 100 &&
    (resetsAt == null || (finite(resetsAt) && resetsAt > now))
  );
}

function hasExhaustedWindow(
  window: RateLimitWindow | undefined | null,
  now: number,
) {
  const resetsAt = window?.resetsAt;
  return finite(resetsAt) && resetsAt <= now;
}

export function limitRecovered(
  agent: Agent,
  limits: Json | null,
  now: number,
): boolean {
  const limitsAt = finite(limits?.at) ? limits.at : Number.NaN;
  if (
    !["usageLimitExceeded", "rateLimitExceeded"].includes(
      nativeErrorKind(agent.error),
    )
  )
    return false;
  const errorAt = finite(agent.nativeLimitErrorAt)
    ? agent.nativeLimitErrorAt
    : typeof agent.lastEvent === "string"
      ? Date.parse(agent.lastEvent) / 1000
      : Number.NaN;
  const data = providerLimitData(limits?.data);
  const rateLimits = data?.rateLimits;
  const buckets = [
    rateLimits,
    ...Object.values(data?.rateLimitsByLimitId ?? {}),
  ];
  if (
    !limits ||
    !freshLimits(limits, agent, now) ||
    limitsAt > now + 5 ||
    (finite(errorAt) && limitsAt <= errorAt) ||
    data?.ordinaryUsageAllowed !== true
  )
    return false;
  return (
    !!rateLimits &&
    buckets.every((bucket) => {
      if (!bucket) return false;
      const remainingPercent = bucket.individualLimit?.remainingPercent;
      return Boolean(
        bucket.rateLimitReachedType == null &&
        (bucket.spendControlReached == null ||
          bucket.spendControlReached === false) &&
        (remainingPercent == null ||
          (finite(remainingPercent) &&
            remainingPercent >= 0 &&
            remainingPercent <= 100)) &&
        !(finite(remainingPercent) && remainingPercent <= 0) &&
        windowAllowsUsage(bucket.primary, now) &&
        windowAllowsUsage(bucket.secondary, now),
      );
    })
  );
}

export function limitRecovery(
  agent: Agent,
  limits: Json | null,
  now: number,
): LimitRecovery | null {
  const kind = nativeErrorKind(agent.error);
  if (limitRecovered(agent, limits, now)) return null;
  if (kind !== "usageLimitExceeded" && kind !== "rateLimitExceeded")
    return null;
  const fallback: LimitRecovery = {
    title: "Usage limit reached",
    message: "View account limits for reset times and available credits.",
  };
  const data = providerLimitData(limits?.data);
  const snapshot = data?.rateLimits;
  if (
    !snapshot ||
    !freshLimits(limits, agent, now) ||
    hasExhaustedWindow(snapshot.primary, now) ||
    hasExhaustedWindow(snapshot.secondary, now)
  )
    return fallback;

  const resets = [snapshot.primary, snapshot.secondary].flatMap((window) => {
    return finite(window?.usedPercent) &&
      window.usedPercent >= 100 &&
      finite(window.resetsAt)
      ? [window.resetsAt]
      : [];
  });
  const reset =
    resets.length > 0 ? { resetAt: Math.max(...resets) } : undefined;
  let reached =
    typeof snapshot.rateLimitReachedType === "string"
      ? snapshot.rateLimitReachedType
      : null;
  if (kind === "usageLimitExceeded") {
    if (reached === "workspace_owner_credits_depleted")
      reached = "workspace_owner_usage_limit_reached";
    if (reached === "workspace_member_credits_depleted")
      reached = "workspace_member_usage_limit_reached";
  }
  switch (reached) {
    case "workspace_owner_credits_depleted":
      return {
        title: "Workspace credits depleted",
        message:
          "Your workspace is out of credits. Add credits to continue using Codex.",
        action: {
          label: "Add workspace credits",
          href: "https://chatgpt.com/admin/billing?codex_credit_action=add_credits",
        },
        ...reset,
      };
    case "workspace_member_credits_depleted":
      return {
        title: "Workspace credits depleted",
        message:
          "Your workspace is out of credits. Ask your workspace owner to add credits.",
        ownerRequest:
          "Codex says our workspace is out of credits. Can you add credits?",
        ...reset,
      };
    case "workspace_owner_usage_limit_reached":
      return {
        title: "Workspace usage limit reached",
        message: "Increase your workspace usage limit to continue using Codex.",
        action: {
          label: "Open workspace usage limits",
          href: "https://chatgpt.com/admin/usage-limits/workspace",
        },
        ...reset,
      };
    case "workspace_member_usage_limit_reached":
      return {
        title: "Workspace usage limit reached",
        message: "Ask your workspace owner to increase your usage limit.",
        ownerRequest:
          "Codex says I reached my workspace usage limit. Can you increase my limit?",
        ...reset,
      };
  }
  // Unknown limit types cannot establish permission to buy or change a plan.
  if (reached != null) return fallback;
  const planType =
    typeof snapshot.planType === "string" ? snapshot.planType : "";
  if (["pro", "plus", "pro_lite"].includes(planType)) {
    return {
      title: "Usage limit reached",
      message: "Wait for the limit to reset, or add credits in ChatGPT.",
      action: {
        label: "View credits in ChatGPT",
        href: "https://chatgpt.com/codex/settings/usage?credits_modal=true",
      },
      ...reset,
    };
  }
  if (["free", "go"].includes(planType)) {
    return {
      title: "Usage limit reached",
      message: "Wait for the limit to reset, or review your plan in ChatGPT.",
      action: {
        label: "View ChatGPT usage",
        href: "https://chatgpt.com/codex/settings/usage",
      },
      ...reset,
    };
  }
  return { ...fallback, ...reset };
}
