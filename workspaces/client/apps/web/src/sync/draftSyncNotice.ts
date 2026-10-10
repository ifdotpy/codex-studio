import { DRAFT_SYNC_TIMING_MS } from "./draftSyncTiming.mjs";

export type DraftSyncFailureDirection = "pull" | "push" | "replication";

export function hasDraftSyncFailure(
  direction: DraftSyncFailureDirection | null,
): boolean {
  return direction !== null;
}

export function draftSyncNoticeText(
  direction: DraftSyncFailureDirection,
  bootstrapPaused: boolean,
): string {
  if (bootstrapPaused)
    return "Draft sync paused. Edit a draft or reconnect to retry.";
  return direction === "pull"
    ? "Draft sync paused. Resumes when reconnected or drafts change."
    : "Draft sync paused. Retrying automatically.";
}

export function scheduleDraftSyncNotice(
  failed: boolean,
  bootstrapPaused: boolean,
  setVisible: (visible: boolean) => void,
): (() => void) | undefined {
  if (!failed) {
    setVisible(false);
    return;
  }
  if (bootstrapPaused) {
    setVisible(true);
    return;
  }
  // Brief network interruptions recover without moving the conversation.
  setVisible(false);
  const timer = setTimeout(
    () => setVisible(true),
    DRAFT_SYNC_TIMING_MS.noticeDelay,
  );
  return () => clearTimeout(timer);
}
