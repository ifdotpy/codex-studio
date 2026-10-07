import { ActionButton } from "../ui/primitives";
import { ArrowLeft } from "lucide-react";
import type { ReactNode } from "react";
import type { Agent } from "../../types";
import ChatStatus from "../agents/ChatStatus";
import type { ChatIndicator } from "../chat-status/chatStatusModel";

export type ConversationTitleProps = {
  title: string;
  agent?: Agent;
  indicator?: ChatIndicator;
  statusText: string;
  projectPrefix?: string;
  onBack?: () => void;
  modeControl?: ReactNode;
};

export default function ConversationTitle({
  title,
  agent,
  indicator,
  statusText,
  projectPrefix = "",
  onBack,
  modeControl,
}: ConversationTitleProps) {
  return (
    <div className="conversation-heading">
      {onBack && (
        <ActionButton
          actionRole="quiet"
          size="compact-xs"
          leftSection={<ArrowLeft size={13} />}
          id="back-lead"
          onClick={onBack}
        >
          Back to main agent
        </ActionButton>
      )}
      <h1 id="conversation-title" title={title}>
        <ChatStatus
          provider={agent?.provider ?? undefined}
          model={agent?.model ?? undefined}
          status={indicator}
        />
        <span className="conversation-title-text">{title}</span>
      </h1>
      <div className="conversation-meta">
        <span id="conversation-status" title={`${projectPrefix}${statusText}`}>
          {projectPrefix}
          {statusText}
        </span>
        {modeControl}
      </div>
    </div>
  );
}
