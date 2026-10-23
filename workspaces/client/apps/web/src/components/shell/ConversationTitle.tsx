import { ActionButton } from "../ui/primitives";
import { ArrowLeft } from "lucide-react";
import { Badge } from "@mantine/core";
import type { Agent } from "../../types";
import type { ChatIndicator } from "../chat-status/chatStatusModel";
import WorkspaceBadge from "./WorkspaceBadge";
import "../conversation/conversation-chrome.css";

export type ConversationTitleProps = {
  title: string;
  agent?: Agent;
  indicator?: ChatIndicator;
  statusText: string;
  projectPrefix?: string;
  onBack?: () => void;
  team?: {
    id: string;
    name: string;
    lead: boolean;
    indicator?: ChatIndicator;
  }[];
  onTeam?: () => void;
};

export default function ConversationTitle({
  title,
  agent,
  indicator,
  statusText,
  projectPrefix = "",
  onBack,
  team = [],
  onTeam,
}: ConversationTitleProps) {
  const status = indicator?.label || statusText;
  const summary = team
    .map(
      (member) =>
        `${member.lead ? "Lead" : "Worker"} ${member.name}: ${member.indicator?.label || "Ready"}`,
    )
    .join("; ");
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
          >
            {status}
          </Badge>
        )}
        <WorkspaceBadge agent={agent} />
        {!!team.length && onTeam && (
          <button
            type="button"
            className="conversation-team-strip"
            onClick={onTeam}
            aria-label={`Open team. ${summary}`}
            title={summary}
          >
            <span>Lead</span>
            {team
              .filter((member) => member.lead)
              .map((member) => (
                <span
                  key={member.id}
                  className="studio-state-dot"
                  data-studio-state={member.indicator?.kind || "none"}
                  aria-hidden="true"
                />
              ))}
            <span>
              Workers{" "}
              <span className="conversation-team-count">
                {team.filter((member) => !member.lead).length}
              </span>
            </span>
            <span className="conversation-worker-dots" aria-hidden="true">
              {team
                .filter((member) => !member.lead)
                .map((member) => (
                  <span
                    key={member.id}
                    className="studio-state-dot"
                    data-studio-state={member.indicator?.kind || "none"}
                  />
                ))}
            </span>
          </button>
        )}
      </div>
    </div>
  );
}
