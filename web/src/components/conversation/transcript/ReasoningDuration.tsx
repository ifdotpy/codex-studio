import { Brain, Clock3 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import type { Message } from "../../../types";
import { useVisualActivity } from "../../../hooks/useVisualActivity";
import "./reasoning-duration.css";

export default function ReasoningDuration({ item }: { item: Message }) {
  const [visualRef, visualActive] = useVisualActivity<HTMLDivElement>();
  const [clock, setClock] = useState<{ start: number; elapsed: number } | null>(
    null,
  );
  const since =
    typeof item.reasoningSince === "number" ? item.reasoningSince : undefined;
  const observedAt =
    typeof item.reasoningObservedAt === "number"
      ? item.reasoningObservedAt
      : undefined;
  const running = since !== undefined;
  const start = useMemo(() => performance.now(), [item.id, since, observedAt]);
  useEffect(() => {
    if (!running || !visualActive) return;
    const update = () =>
      setClock({ start, elapsed: (performance.now() - start) / 1000 });
    update();
    const timer = window.setInterval(update, 1000);
    return () => window.clearInterval(timer);
  }, [running, start, visualActive]);
  const elapsed = clock?.start === start ? clock.elapsed : 0;
  const seconds = Math.max(
    0,
    Math.floor(
      (item.reasoningMs || 0) / 1000 +
        (running ? Math.max(0, (observedAt ?? since) - since) + elapsed : 0),
    ),
  );
  const duration =
    seconds < 60
      ? `${seconds}s`
      : `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  const Icon = item.observedWait ? Clock3 : Brain;
  return (
    <div
      ref={visualRef}
      className="reasoning-duration"
      data-message={item.id}
      data-running={running}
    >
      <Icon size={14} aria-hidden="true" />
      <span>
        {running ? "Thinking" : item.observedWait ? "Waited" : "Thought"}{" "}
        <span className="reasoning-time">{duration}</span>
      </span>
    </div>
  );
}
