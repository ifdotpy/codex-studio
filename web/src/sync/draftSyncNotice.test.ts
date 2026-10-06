import { afterEach, describe, expect, it, vi } from "vitest";
import {
  draftSyncNoticeText,
  hasDraftSyncFailure,
  scheduleDraftSyncNotice,
  type DraftSyncFailureDirection,
} from "./draftSyncNotice";

describe("draft sync failure notice", () => {
  afterEach(() => vi.useRealTimers());

  it.each([
    ["pull", "push", "Draft sync paused. Retrying automatically."],
    [
      "push",
      "pull",
      "Draft sync paused. Resumes when reconnected or drafts change.",
    ],
  ] as const)(
    "does not restart the timer when %s failure changes to %s",
    (initial, next, expected) => {
      vi.useFakeTimers();
      let direction: DraftSyncFailureDirection = initial;
      let visible = false;
      const initialFailed = hasDraftSyncFailure(direction);
      const stopTimer = scheduleDraftSyncNotice(
        initialFailed,
        false,
        (value) => {
          visible = value;
        },
      );

      vi.advanceTimersByTime(4000);
      direction = next;
      const nextFailed = hasDraftSyncFailure(direction);
      expect(nextFailed).toBe(initialFailed);
      vi.advanceTimersByTime(4000);
      expect(visible).toBe(true);
      expect(draftSyncNoticeText(direction, false)).toBe(expected);
      stopTimer?.();
    },
  );
});
