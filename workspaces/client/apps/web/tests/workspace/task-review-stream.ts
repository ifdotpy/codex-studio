import { useSyncExternalStore } from "react";
import type { SubscriptionState } from "../../src/sync/subscriptions/contracts.ts";
import type { components } from "../../src/generated/api.ts";
export type TaskReviewFrames = {
  [Kind in "agentDetail" | "work"]: SubscriptionState<
    components["schemas"]["SubscriptionDataMap"][Kind]
  >;
};
declare global {
  interface Window {
    taskReviewFrames: TaskReviewFrames;
  }
}
const subscribe = (accept: () => void) => {
  window.addEventListener("task-review-frame", accept);
  return () => window.removeEventListener("task-review-frame", accept);
};
export const useSubscription = (ref: { kind: keyof TaskReviewFrames }) =>
  useSyncExternalStore(subscribe, () => window.taskReviewFrames[ref.kind]);
