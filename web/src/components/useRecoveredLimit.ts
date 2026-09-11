import { useEffect, useRef } from "react";
import { saved, save } from "../api";
import { limitRecovered } from "../limitRecovery";
import type { Agent, Json } from "../types";

export function useRecoveredLimit(agent: Agent, limits: Json | null, now: number) {
  const storageKey = `studio-recovered-limit:${JSON.stringify([
    agent.id, agent.accountKey, agent.threadId,
  ])}`;
  const episode = JSON.stringify([
    agent.nativeLimitErrorAt ?? [agent.lastCompletedTurn, agent.turnId, agent.lastEvent],
    agent.error,
  ]);
  const fresh = limitRecovered(agent, limits, now);
  const recovered = useRef("");
  const key = storageKey + episode;
  if (fresh || saved(storageKey, "") === episode) recovered.current = key;
  useEffect(() => {
    // Keep a confirmed recovery when the chat remounts or its next read fails.
    // This changes presentation only, not the saved error or permission to retry.
    if (fresh) save(storageKey, episode);
  }, [storageKey, episode, fresh]);
  return recovered.current === key;
}
