import { useEffect, useRef, useState } from "react";
import type { Agent } from "../types";
import {
  formatTokenRate,
  subscribeTokenRate,
  tweenTokenRate,
  workerRateKey,
} from "../tokenRate";
import type { TokenRate as Rate } from "../tokenRate";

import "./TokenRate.css";

export default function TokenRate({
  agent,
  variant = "footer",
}: {
  agent: Agent;
  variant?: "footer" | "worker";
}) {
  const scope =
    variant === "worker"
      ? workerRateKey(agent.rootId || "", agent.id)
      : agent.id;
  const [sample, setSample] = useState<{
    id: string;
    value: Rate | null;
  } | null>(null);
  const [motionReduced, setMotionReduced] = useState(
    () => window.matchMedia("(prefers-reduced-motion: reduce)").matches,
  );
  const [shown, setShown] = useState(0);
  const current = useRef(0);
  const previousTurn = useRef("");
  useEffect(
    () => subscribeTokenRate(scope, (value) => setSample({ id: scope, value })),
    [scope],
  );
  useEffect(() => {
    const media = window.matchMedia("(prefers-reduced-motion: reduce)");
    const update = () => setMotionReduced(media.matches);
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  const rate = sample?.id === scope ? sample.value : null;
  // Card metadata can lag the transcript. The batch owns its current turn.
  const matchesTurn =
    variant === "worker" || !agent.turnId || rate?.turnId === agent.turnId;
  const active = Boolean(
    rate?.active &&
      (variant === "worker" ||
        (agent.inFlight && ["starting", "running"].includes(agent.status))),
  );
  const visible =
    matchesTurn &&
    rate !== null &&
    rate.outputTokens > 0 &&
    (variant === "footer" || (active && rate.rate > 0));
  const target = visible ? rate.rate : 0;
  const turn = `${agent.id}:${variant === "worker" ? rate?.turnId || "" : agent.turnId || rate?.turnId || ""}`;
  useEffect(() => {
    // The first sample of a new turn must not tween from the previous turn.
    if (previousTurn.current !== turn || motionReduced || !visible) {
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
  }, [target, turn, motionReduced, visible]);
  return (
    <span
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
          ? `${rate?.estimated ? "Estimate from streamed text. " : "Provider output token count. "}Average over the last four seconds.${active ? "" : " Last turn."}`
          : undefined
      }
    >
      {visible
        ? `${rate?.estimated ? "≈" : ""}${formatTokenRate(previousTurn.current === turn ? shown : target)} tok/s`
        : ""}
    </span>
  );
}
