import { ActionButton } from "../ui/primitives";
import { ArrowLeft } from "lucide-react";
import type { Agent } from "../../types";
import WorkspaceBadge from "./WorkspaceBadge";
import { ChatServerLine } from "../../servers/ProjectChatRows";
import "../conversation/conversation-chrome.css";

export type ConversationTitleProps = {
  title: string;
  passive?: boolean;
  projectName?: string;
  agent?: Agent;
  serverAlias?: string;
  serverOffline?: boolean;
  onBack?: () => void;
};

export default function ConversationTitle({
  title,
  passive = false,
  projectName,
  agent,
  serverAlias,
  serverOffline = false,
  onBack,
}: ConversationTitleProps) {
  return (
    <div className="conversation-heading">
      {!passive && onBack && (
        <ActionButton
          actionRole="quiet"
          size="compact-xs"
          leftSection={<ArrowLeft size={13} />}
          id="back-lead"
          aria-label="Back to main agent"
          title="Back to main agent"
          onClick={onBack}
        >
          <span className="conversation-back-label">Back to main agent</span>
        </ActionButton>
      )}
      <h1 id="conversation-title" title={title}>
        <span className="conversation-title-text">{title}</span>
      </h1>
      <div className="conversation-meta">
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
        <WorkspaceBadge agent={agent} passive={passive} />
        {projectName && (
          <span className="conversation-project">{projectName}</span>
        )}
      </div>
    </div>
  );
}
