import type { UsageEligibility } from "../components/ModelLimitEligibility";
import type { Agent, Json } from "../types";
import { nativeErrorKind } from "../nativeErrors";
import { providerLimitData } from "./providerLimits";

export type LimitRecovery = {
  title: string;
  message: string;
  resetAt?: number;
  eligibility?: UsageEligibility | undefined;
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

export function modelEligibility(
  agent: Agent,
  limits: Json | null,
): UsageEligibility | undefined {
  const value = limits?.eligibility;
  if (!value || typeof value !== "object" || Array.isArray(value))
    return undefined;
  if (
    value.agentId !== agent.id ||
    value.accountKey !== (agent.accountKey || "default") ||
    value.model !== (agent.model || "") ||
    value.provider !== (agent.provider || "")
  )
    return undefined;
  return value as UsageEligibility;
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
  const eligibility = modelEligibility(agent, limits);
  return Boolean(
    limits &&
    freshLimits(limits, agent, now) &&
    limitsAt <= now + 5 &&
    (!finite(errorAt) || limitsAt > errorAt) &&
    eligibility?.canRun &&
    eligibility.fresh &&
    eligibility.validUntil != null &&
    now < eligibility.validUntil,
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
  const eligibility = modelEligibility(agent, limits);
  const fallback: LimitRecovery = {
    ...(eligibility ? { eligibility } : {}),
    title: "Usage limit reached",
    message: "View account limits for reset times and available credits.",
  };
  const data = providerLimitData(limits?.data);
  const snapshot = data?.rateLimits;
  if (!snapshot || !freshLimits(limits, agent, now)) return fallback;

  const reset =
    eligibility?.resetAt != null ? { resetAt: eligibility.resetAt } : undefined;
  if (agent.provider === "claude") return { ...fallback, ...reset };
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
        eligibility,
        ...reset,
      };
    case "workspace_member_credits_depleted":
      return {
        title: "Workspace credits depleted",
        message:
          "Your workspace is out of credits. Ask your workspace owner to add credits.",
        ownerRequest:
          "Codex says our workspace is out of credits. Can you add credits?",
        eligibility,
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
        eligibility,
        ...reset,
      };
    case "workspace_member_usage_limit_reached":
      return {
        title: "Workspace usage limit reached",
        message: "Ask your workspace owner to increase your usage limit.",
        ownerRequest:
          "Codex says I reached my workspace usage limit. Can you increase my limit?",
        eligibility,
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
      eligibility,
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
      eligibility,
      ...reset,
    };
  }
  return { ...fallback, ...reset };
}
