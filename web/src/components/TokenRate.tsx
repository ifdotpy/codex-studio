import { useEffect, useRef, useState } from "react";
import type { Agent } from "../types";
import {
  formatTokenRate,
  subscribeTokenRate,
  tweenTokenRate,
} from "../tokenRate";
import type { TokenRate as Rate } from "../tokenRate";

export default function TokenRate({ agent }: { agent: Agent }) {
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
    () =>
      subscribeTokenRate(agent.id, (value) =>
        setSample({ id: agent.id, value }),
      ),
    [agent.id],
  );
  useEffect(() => {
    const media = window.matchMedia("(prefers-reduced-motion: reduce)");
    const update = () => setMotionReduced(media.matches);
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  const rate = sample?.id === agent.id ? sample.value : null;
  const matchesTurn = !agent.turnId || rate?.turnId === agent.turnId;
  const visible = matchesTurn && rate !== null && rate.outputTokens > 0;
  const target = visible ? rate.rate : 0;
  const turn = `${agent.id}:${agent.turnId || rate?.turnId || ""}`;
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
  const active = Boolean(rate?.active && agent.inFlight);
  return (
    <span
      className="token-rate"
      data-testid="token-rate"
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
