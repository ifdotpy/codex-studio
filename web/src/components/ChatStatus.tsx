import { CircleAlert, LoaderCircle, Pause } from "lucide-react";
import type { ChatIndicator } from "./chatStatusModel";
import "./chat-status.css";

export default function ChatStatus({ status }: { status?: ChatIndicator }) {
  if (!status || status.kind === "none") return null;
  return (
    <span
      className={`chat-status chat-status-${status.kind}`}
      data-chat-status={status.kind}
      role="img"
      aria-label={status.label}
      title={status.label}
    >
      {status.kind === "working" ? (
        <LoaderCircle size={13} />
      ) : status.kind === "error" ? (
        <CircleAlert size={13} />
      ) : status.kind === "paused" ? (
        <Pause size={12} />
      ) : (
        <span className="chat-status-dot" />
      )}
    </span>
  );
}
