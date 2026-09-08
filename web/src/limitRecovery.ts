import type { Agent, Json } from "./types";
import { nativeErrorKind } from "./nativeErrors";

export type LimitRecovery = {
  title: string;
  message: string;
  resetAt?: number;
  action?: { label: string; href: string };
  ownerRequest?: string;
};
const finite = (value: unknown): value is number =>
  typeof value === "number" && Number.isFinite(value);

export function limitRecovery(
  agent: Agent,
  limits: Json | null,
  now: number,
): LimitRecovery | null {
  const kind = nativeErrorKind(agent.error);
  if (kind !== "usageLimitExceeded" && kind !== "rateLimitExceeded")
    return null;
  const fallback: LimitRecovery = {
    title: "Usage limit reached",
    message:
      "Refresh the account limits. Check the reset time and available reset credits below.",
  };
  const snapshot = limits?.data?.rateLimits;
  if (
    !snapshot ||
    limits?.error ||
    limits?.stale ||
    limits?.loading ||
    (limits?.accountKey || "default") !== (agent.accountKey || "default") ||
    !finite(limits?.at) ||
    now - limits.at > 300 ||
    [snapshot.primary, snapshot.secondary].some(
      (window) => finite(window?.resetsAt) && window.resetsAt <= now,
    )
  )
    return fallback;

  const resets = [snapshot.primary, snapshot.secondary]
    .filter(
      (window) =>
        finite(window?.usedPercent) &&
        window.usedPercent >= 100 &&
        finite(window?.resetsAt) &&
        window.resetsAt > now,
    )
    .map((window) => window.resetsAt as number);
  const reset = resets.length ? { resetAt: Math.max(...resets) } : {};
  let reached = snapshot.rateLimitReachedType;
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
  if (["pro", "plus", "pro_lite"].includes(snapshot.planType)) {
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
  if (["free", "go"].includes(snapshot.planType)) {
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
