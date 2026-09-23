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
  if (!status || status.kind === "none" || status.kind === "paused") return null;
  const kind = status.kind;
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
      {kind === "working" ? (
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
