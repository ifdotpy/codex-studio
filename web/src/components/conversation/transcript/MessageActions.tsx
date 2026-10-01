import { ActionIcon, Loader, Tooltip } from "@mantine/core";
import { Copy, GitBranch, Pencil, Quote, RotateCcw } from "lucide-react";

const messageActionTooltipEvents = { hover: true, focus: true, touch: false };

export type MessageActionsProps = {
  hidden?: boolean;
  onCopy: () => void;
  onQuote?: () => void;
  onEdit?: () => void;
  editLoading?: boolean;
  editDisabled?: boolean;
  onBranch?: () => void;
  branchDisabled?: boolean;
  onAnotherAnswer?: () => void;
  anotherAnswerDisabled?: boolean;
};

export default function MessageActions({
  hidden = false,
  onCopy,
  onQuote,
  onEdit,
  editLoading = false,
  editDisabled = false,
  onBranch,
  branchDisabled = false,
  onAnotherAnswer,
  anotherAnswerDisabled = false,
}: MessageActionsProps) {
  return (
    <div className="message-bottom" hidden={hidden}>
      <Tooltip
        label="Copy this message"
        position="top"
        withArrow
        events={messageActionTooltipEvents}
      >
        <ActionIcon
          size="sm"
          className="copy-message"
          aria-label="Copy message"
          onClick={onCopy}
        >
          <Copy size={14} />
        </ActionIcon>
      </Tooltip>
      {onQuote && (
        <Tooltip
          label="Quote this message in your reply"
          position="top"
          withArrow
          events={messageActionTooltipEvents}
        >
          <ActionIcon
            size="sm"
            aria-label="Quote message"
            onPointerDown={(event) => event.preventDefault()}
            onClick={onQuote}
          >
            <Quote size={14} />
          </ActionIcon>
        </Tooltip>
      )}
      {onEdit && (
        <Tooltip
          label="Edit this message in a new chat"
          position="top"
          withArrow
          events={messageActionTooltipEvents}
        >
          <span
            style={{ display: "inline-flex" }}
            role={editDisabled ? "group" : undefined}
            aria-label={editDisabled ? "Edit in a new chat" : undefined}
            tabIndex={editDisabled ? 0 : undefined}
          >
            <ActionIcon
              size="sm"
              aria-label="Edit in a new chat"
              disabled={editDisabled}
              onClick={onEdit}
            >
              {editLoading ? <Loader size={14} /> : <Pencil size={14} />}
            </ActionIcon>
          </span>
        </Tooltip>
      )}
      {onBranch && (
        <Tooltip
          label="Start a new branch after this turn"
          position="top"
          withArrow
          events={messageActionTooltipEvents}
        >
          <span
            style={{ display: "inline-flex" }}
            role={branchDisabled ? "group" : undefined}
            aria-label={branchDisabled ? "Branch after this turn" : undefined}
            tabIndex={branchDisabled ? 0 : undefined}
          >
            <ActionIcon
              size="sm"
              aria-label="Branch after this turn"
              disabled={branchDisabled}
              onClick={onBranch}
            >
              <GitBranch size={14} />
            </ActionIcon>
          </span>
        </Tooltip>
      )}
      {onAnotherAnswer && (
        <Tooltip
          label="Prepare a request for another answer in a new chat"
          position="top"
          withArrow
          events={messageActionTooltipEvents}
        >
          <span
            style={{ display: "inline-flex" }}
            role={anotherAnswerDisabled ? "group" : undefined}
            aria-label={
              anotherAnswerDisabled ? "Another answer in a new chat" : undefined
            }
            tabIndex={anotherAnswerDisabled ? 0 : undefined}
          >
            <ActionIcon
              size="sm"
              aria-label="Another answer in a new chat"
              disabled={anotherAnswerDisabled}
              onClick={onAnotherAnswer}
            >
              <RotateCcw size={14} />
            </ActionIcon>
          </span>
        </Tooltip>
      )}
    </div>
  );
}
