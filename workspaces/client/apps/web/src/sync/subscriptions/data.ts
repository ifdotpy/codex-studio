import type { ResourceData, ResourceRef } from "./contracts";
import type { ObserveSubscriptionData } from "./module";
import type { ResourceVersion } from "../resourceEvents";
import type { createSyncClient } from "../client";
import type { SubscriptionTransport } from "./store";

/** Projection observers read the existing local store; they never issue a GET. */
export function createDataObserver(
  sync: () => Pick<
    ReturnType<typeof createSyncClient>,
    "subscribeTranscriptProjection"
  >,
  events: SubscriptionTransport,
  observeEntities?: ObserveSubscriptionData,
): ObserveSubscriptionData {
  return <R extends ResourceRef>(
    ref: R,
    accept: (data: ResourceData<R>) => void,
    failed: (error: unknown) => void,
  ) => {
    if (ref.kind === "transcript") {
      return sync().subscribeTranscriptProjection(
        `transcript:${ref.agentId}`,
        (data) => {
          if (data !== null) accept(data as ResourceData<R>);
        },
        (error) => {
          if (error !== null) failed(error);
        },
      );
    }
    const transient = (version?: ResourceVersion) => {
      const data =
        version?.progress?.value ??
        version?.discoverySnapshot ??
        version?.voiceRecords ??
        version?.draftChanges;
      if (data !== undefined) accept(data as ResourceData<R>);
    };
    const stop = events.watchResourceChanges(ref, transient);
    const stopEntities = observeEntities?.(ref, accept, failed);
    return () => {
      stop();
      stopEntities?.();
    };
  };
}
