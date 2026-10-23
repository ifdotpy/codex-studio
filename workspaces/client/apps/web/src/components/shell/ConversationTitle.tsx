import { ActionButton } from "../ui/primitives";
import { ArrowLeft } from "lucide-react";
import { Badge } from "@mantine/core";
import type { Agent } from "../../types";
import type { ChatIndicator } from "../chat-status/chatStatusModel";
import WorkspaceBadge from "./WorkspaceBadge";
import { ChatServerLine } from "../../servers/ProjectChatRows";
import "../conversation/conversation-chrome.css";

export type ConversationTitleProps = {
  title: string;
  agent?: Agent;
  indicator?: ChatIndicator;
  statusText: string;
  projectPrefix?: string;
  serverAlias?: string;
  serverOffline?: boolean;
  onBack?: () => void;
};

export default function ConversationTitle({
  title,
  agent,
  indicator,
  statusText,
  projectPrefix = "",
  serverAlias,
  serverOffline = false,
  onBack,
}: ConversationTitleProps) {
  const status = indicator?.label || statusText;
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
        <span className="conversation-title-text">{title}</span>
      </h1>
      <div className="conversation-meta">
        {status && (
          <Badge
            id="conversation-status"
            className="conversation-state"
            data-studio-state={indicator?.kind || "none"}
            variant="light"
            radius="sm"
            title={`${projectPrefix}${status}`}
            aria-label={status}
          >
            <span className="conversation-status-label">{status}</span>
            <span className="conversation-status-dot" aria-hidden="true" />
          </Badge>
        )}
        {serverAlias && (
          <span
            className="conversation-server"
            data-offline={serverOffline}
            aria-label={serverOffline ? `${serverAlias} offline` : undefined}
          >
            <ChatServerLine
              provider={agent?.provider ?? undefined}
              alias={serverAlias}
              offline={serverOffline}
            />
          </span>
        )}
        <WorkspaceBadge agent={agent} />
      </div>
    </div>
  );
}
