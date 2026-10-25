import { Button, Modal } from "@mantine/core";

export function ProjectDeleteDialog({
  name,
  phase,
  error,
  onCancel,
  onConfirm,
  onRecover,
}: {
  name: string | null;
  phase: "confirming" | "submitting" | "unknown" | "rejected";
  error: string;
  onCancel: () => void;
  onConfirm: () => void;
  onRecover: () => void;
}) {
  const locked = phase === "submitting" || phase === "unknown";
  return (
    <Modal
      opened={name !== null}
      onClose={onCancel}
      title={
        phase === "unknown" ? "Delete result unconfirmed" : "Delete project"
      }
      closeOnClickOutside={!locked}
      closeOnEscape={!locked}
      withCloseButton={!locked}
    >
      <p>
        Delete project <strong>{name}</strong>?
      </p>
      <p>
        All its chats, archived chats, workers and shared rooms will be deleted.
        Running agents will be stopped.
      </p>
      <p>
        History, cost accounting, the project folder and Git worktrees are kept.
        Disposable managed images and virtual machine worker lines may be
        removed.
      </p>
      <p>This cannot be undone in Studio.</p>
      {error && <p role="alert">{error}</p>}
      <div className="dialog-actions">
        <Button
          variant="default"
          disabled={locked}
          onClick={onCancel}
          data-autofocus
        >
          {phase === "rejected" ? "Close" : "Cancel"}
        </Button>
        {phase === "unknown" ? (
          <Button onClick={onRecover}>Recover saved request</Button>
        ) : (
          phase !== "rejected" && (
            <Button
              color="red"
              loading={phase === "submitting"}
              disabled={phase === "submitting"}
              onClick={onConfirm}
            >
              Delete project
            </Button>
          )
        )}
      </div>
    </Modal>
  );
}
