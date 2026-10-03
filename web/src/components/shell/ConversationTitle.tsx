import { Button } from "@mantine/core";
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
        <Button
          size="compact-xs"
          leftSection={<ArrowLeft size={13} />}
          id="back-lead"
          onClick={onBack}
        >
          Back to main agent
        </Button>
      )}
      <h1 id="conversation-title" title={title}>
        <ChatStatus
          provider={agent?.provider}
          model={agent?.model}
          status={indicator}
        />
        {title}
      </h1>
      <div className="conversation-meta">
        <span id="conversation-status">
          {projectPrefix}
          {statusText}
        </span>
        {modeControl}
      </div>
    </div>
  );
}
