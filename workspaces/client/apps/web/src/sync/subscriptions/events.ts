/** Internal adapters for projection, recovery and non-React consumers. Components
 * use useSubscription; transport details stay within the subscription boundary. */
import * as transport from "../resourceEvents";
import { subscribeTranscriptProjection } from "../client";
import { createSubscriptions } from "./module";
import { createDataObserver } from "./data";

const subscriptions = createSubscriptions(
  transport,
  createDataObserver(() => ({ subscribeTranscriptProjection }), transport),
);
export const {
  watchResourceChanges,
  watchResourceConnection,
  watchTokenRateEvents,
  acknowledgeEntitySequences,
} = subscriptions;
export const defaultSubscriptions = subscriptions;
export { createSubscriptions } from "./module";
export { createDataObserver } from "./data";
export type * from "../resourceEvents";
