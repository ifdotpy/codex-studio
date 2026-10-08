import { useEffect, useRef, useState } from "react";
import type { Agent } from "../types";
import {
  formatTokenRate,
  getTokenRate,
  getWorkspaceTeamTokenRate,
  subscribeTokenRate,
  tweenTokenRate,
  workerRateKey,
} from "../usage/tokenRate";
import type { TokenRate as Rate } from "../usage/tokenRate";
import { useVisualActivity } from "../hooks/useVisualActivity";

import "./TokenRate.css";

export default function TokenRate({
  agent,
  variant = "footer",
}: {
  agent: Agent;
  variant?: "footer" | "worker";
}) {
  const [visualRef, visualActive] = useVisualActivity<HTMLSpanElement>();
  const scope =
    variant === "worker"
      ? workerRateKey(agent.rootId || "", agent.id)
      : agent.id;
  const workspaceTeamRate =
    variant === "worker"
      ? getWorkspaceTeamTokenRate(agent.rootId || "", agent.id)
      : null;
  const initialRate = workspaceTeamRate || getTokenRate(scope);
  const [sample, setSample] = useState<{
    id: string;
    value: Rate | null;
  }>(() => ({ id: scope, value: initialRate }));
  const [motionReduced, setMotionReduced] = useState(
    () => window.matchMedia("(prefers-reduced-motion: reduce)").matches,
  );
  const [shown, setShown] = useState(0);
  const current = useRef(0);
  const previousTurn = useRef("");
  useEffect(
    () =>
      visualActive
        ? subscribeTokenRate(scope, (value) => setSample({ id: scope, value }))
        : undefined,
    [scope, visualActive],
  );
  useEffect(() => {
    const media = window.matchMedia("(prefers-reduced-motion: reduce)");
    const update = () => setMotionReduced(media.matches);
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  const sampleRate = sample?.id === scope ? sample.value : null;
  const rate =
    (variant === "worker" ? workspaceTeamRate : getTokenRate(scope)) ||
    sampleRate;
  // Card metadata can lag the transcript. The batch owns its current turn.
  const matchesTurn =
    variant === "worker" || !agent.turnId || rate?.turnId === agent.turnId;
  const active = Boolean(rate?.active);
  const visible =
    matchesTurn &&
    rate !== null &&
    rate.outputTokens > 0 &&
    (!active || rate.rate > 0);
  const target = visible ? rate.rate : 0;
  const turn = `${agent.id}:${variant === "worker" ? rate?.turnId || "" : agent.turnId || rate?.turnId || ""}`;
  useEffect(() => {
    if (!visualActive) return;
    // The first sample of a new turn must not tween from the previous turn.
    if (
      previousTurn.current !== turn ||
      motionReduced ||
      !visible ||
      !current.current
    ) {
      previousTurn.current = turn;
      current.current = target;
      setShown(target);
      return;
    }
    const from = current.current;
    const started = performance.now();
    let frame: number;
    const tick = (now: number) => {
      const value = tweenTokenRate(from, target, now - started);
      current.current = value;
      setShown(value);
      if (now - started < 450) frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [target, turn, motionReduced, visible, visualActive]);
  return (
    <span
      ref={visualRef}
      className="token-rate"
      data-testid="token-rate"
      data-variant={variant}
      data-agent={agent.id}
      data-active={active}
      data-estimated={rate?.estimated || false}
      data-reduced-motion={motionReduced}
      data-rate={visible ? target : ""}
      aria-label={
        visible
          ? `${rate?.estimated ? "Estimated " : ""}output tokens per second${active ? "" : ", last turn"}`
          : undefined
      }
      title={
        visible
          ? `${rate?.estimated ? "Estimate from recent text. Hidden reasoning can change the provider rate." : "Provider output tokens, including reasoning, over the observed response interval. Tool time is excluded."}${active ? "" : " Last turn."}`
          : undefined
      }
    >
      {visible
        ? `${rate?.estimated ? "≈" : ""}${formatTokenRate(previousTurn.current === turn && shown > 0 ? shown : target)} tok/s`
        : ""}
    </span>
  );
}
