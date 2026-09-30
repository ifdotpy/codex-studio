import { ActionIcon, Loader } from "@mantine/core";
import { Copy, GitBranch, Pencil, Quote, RotateCcw } from "lucide-react";

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
      <ActionIcon
        size="sm"
        className="copy-message"
        aria-label="Copy message"
        onClick={onCopy}
      >
        <Copy size={14} />
      </ActionIcon>
      {onQuote && (
        <ActionIcon
          size="sm"
          aria-label="Quote message"
          onPointerDown={(event) => event.preventDefault()}
          onClick={onQuote}
        >
          <Quote size={14} />
        </ActionIcon>
      )}
      {onEdit && (
        <ActionIcon
          size="sm"
          aria-label="Edit in a new chat"
          title="Edit in a new chat"
          disabled={editDisabled}
          onClick={onEdit}
        >
          {editLoading ? <Loader size={14} /> : <Pencil size={14} />}
        </ActionIcon>
      )}
      {onBranch && (
        <ActionIcon
          size="sm"
          aria-label="Branch after this turn"
          disabled={branchDisabled}
          onClick={onBranch}
        >
          <GitBranch size={14} />
        </ActionIcon>
      )}
      {onAnotherAnswer && (
        <ActionIcon
          size="sm"
          aria-label="Another answer in a new chat"
          title="Another answer in a new chat"
          disabled={anotherAnswerDisabled}
          onClick={onAnotherAnswer}
        >
          <RotateCcw size={14} />
        </ActionIcon>
      )}
    </div>
  );
}
