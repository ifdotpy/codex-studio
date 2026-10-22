import { useEffect, useState } from "react";
import { useServerApi } from "../servers/ServerServicesContext";
import type { GetResult } from "../api";
import type { Agent, Json } from "../types";

/** Account caches are shared. Keep the Rust model decision local to this agent. */
export function useLimitEligibility(agent: Agent, limits: Json | null) {
  const { get } = useServerApi();
  const key = JSON.stringify([
    agent.id,
    agent.accountKey,
    agent.provider,
    agent.model,
    limits?.at,
    limits?.checkedAt,
    limits?.error,
    limits?.stale,
  ]);
  const [result, setResult] = useState<{
    key: string;
    limits: GetResult<"/api/limits">;
  } | null>(null);
  useEffect(() => {
    if (!agent.id || !limits || limits.eligibility) return;
    let active = true;
    void get("/api/limits", {
      query: {
        account_key: agent.accountKey || "default",
        cached: "1",
        agent: agent.id,
      },
    })
      .then((value) => {
        if (active && value) setResult({ key, limits: value });
      })
      .catch(() => {
        if (active) setResult(null);
      });
    return () => {
      active = false;
    };
  }, [get, key]);
  return result?.key === key ? result.limits : limits;
}
