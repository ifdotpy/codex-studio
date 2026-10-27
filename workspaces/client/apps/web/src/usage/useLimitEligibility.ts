import type { Agent } from "../types";
import type { AccountLimitsSnapshot } from "./accountUsage";

/** Eligibility is derived from the streamed account entity and agent state. */
export function useLimitEligibility(
  _agent: Agent,
  limits: AccountLimitsSnapshot | null,
  _enabled = true,
) {
  return limits;
}
