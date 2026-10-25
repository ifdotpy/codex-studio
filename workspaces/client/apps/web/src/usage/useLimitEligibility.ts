import type { Agent, Json } from "../types";

/** Eligibility is derived from the streamed account entity and agent state. */
export function useLimitEligibility(
  _agent: Agent,
  limits: Json | null,
  _enabled = true,
) {
  return limits;
}
