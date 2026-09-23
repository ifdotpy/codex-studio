import { CircleAlert } from "lucide-react";
import type { ChatIndicator } from "./chatStatusModel";
import "./chat-status.css";
import "./provider-activity.css";

export default function ChatStatus({
  status,
  provider,
  model,
}: {
  status?: ChatIndicator;
  provider?: string;
  model?: string;
}) {
  if ((!status || status.kind === "none") && !model) return null;
  const kind = status?.kind === "none" || !status ? "idle" : status.kind;
  const family = model
    ?.toLowerCase()
    .match(/astra|sol|luna|terra|opus|sonnet|haiku|fable/)?.[0];
  const angles: Record<string, number> = {
    astra: 0,
    opus: 0,
    sol: 90,
    sonnet: 90,
    luna: 180,
    haiku: 180,
    terra: 270,
    fable: 270,
  };
  const angle = family ? angles[family] : 45;
  const modelLabel = model
    ? `${provider === "claude" ? "Claude" : "Codex"} · ${model}`
    : "";
  const label = [status?.label, modelLabel].filter(Boolean).join(" · ");
  return (
    <span
      className={`chat-status chat-status-${kind}`}
      data-chat-status={kind}
      data-agent-provider={provider || "codex"}
      role="img"
      aria-label={label}
      title={label}
    >
      {kind === "working" || kind === "idle" || kind === "paused" ? (
        <svg
          width="13"
          height="13"
          viewBox="0 0 16 16"
          fill="none"
          aria-hidden="true"
        >
          <circle
            cx="8"
            cy="8"
            r="5.5"
            pathLength="100"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinecap="round"
            strokeDasharray="76 24"
            transform={`rotate(${angle} 8 8)`}
          />
        </svg>
      ) : kind === "error" ? (
        <CircleAlert size={13} />

      ) : (
        <span className="chat-status-dot" />
      )}
    </span>
  );
}
