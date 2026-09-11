import { Brain, Clock3 } from "lucide-react";
import { useEffect, useState } from "react";
import type { Message } from "../types";
import "./reasoning-duration.css";

export default function ReasoningDuration({ item }: { item: Message }) {
  const [elapsed, setElapsed] = useState(0);
  const running = typeof item.reasoningSince === "number";
  useEffect(() => {
    setElapsed(0);
    if (!running) return;
    const start = performance.now();
    const timer = window.setInterval(
      () => setElapsed((performance.now() - start) / 1000),
      1000,
    );
    return () => window.clearInterval(timer);
  }, [running, item.reasoningSince, item.reasoningObservedAt]);
  const seconds = Math.max(
    0,
    Math.floor(
      (item.reasoningMs || 0) / 1000 +
        (running
          ? Math.max(0, item.reasoningObservedAt - item.reasoningSince) +
            elapsed
          : 0),
    ),
  );
  const duration =
    seconds < 60
      ? `${seconds}s`
      : `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  const Icon = item.observedWait ? Clock3 : Brain;
  return (
    <div
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
