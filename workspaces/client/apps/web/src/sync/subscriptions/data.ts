import { watchResourceReads } from "./reads";
import type { ResourceData, ResourceRef } from "./contracts";
import type { ObserveSubscriptionData } from "./module";
import type { ResourceVersion } from "../resourceEvents";
import type { createSyncClient } from "../client";
import type { SubscriptionTransport } from "./store";

/** Apply inline stream values; only oversized/missing recovery frames may read HTTP. */
export function createDataObserver(
  sync: () => Pick<
    ReturnType<typeof createSyncClient>,
    "subscribeTranscriptProjection"
  >,
  events: SubscriptionTransport,
  observeEntities?: ObserveSubscriptionData,
  recoverView?: (ref: ResourceRef) => Promise<unknown>,
): ObserveSubscriptionData {
  return <R extends ResourceRef>(
    ref: R,
    accept: (
      data: ResourceData<R> | undefined,
      appliedVersion?: ResourceVersion,
    ) => void,
    failed: (error: unknown) => void,
  ) => {
    if (ref.kind === "transcript") {
      return sync().subscribeTranscriptProjection(
        `transcript:${ref.agentId}`,
        (data) => {
          accept(data === null ? undefined : (data as ResourceData<R>));
        },
        (error) => {
          if (error !== null) failed(error);
        },
        true,
        ref,
      );
    }
    const viewValue = (version?: ResourceVersion) => {
      const view = version?.viewSnapshot;
      const viewData =
        ref.kind === "processes"
          ? view?.processes
          : ref.kind === "complaint"
            ? view?.complaint
            : ref.kind === "room"
              ? (view?.room ?? view?.legacyMessages)
              : ref.kind === "terminal-output"
                ? view?.terminalOutput
                : undefined;
      return viewData;
    };
    if (
      recoverView &&
      ["processes", "complaint", "room", "terminal-output"].includes(ref.kind)
    ) {
      let stopped = false;
      const stop = watchResourceReads(
        ref,
        async (version) => {
          const value = viewValue(version) ?? (await recoverView(ref));
          if (!stopped) accept(value as ResourceData<R>, version);
        },
        failed,
        { events, waitForBaseline: true, reapplyInlineView: true },
      );
      return () => {
        stopped = true;
        stop();
      };
    }
    const transient = (version?: ResourceVersion) => {
      const viewData = viewValue(version);
      if (viewData !== undefined) {
        accept(viewData as ResourceData<R>, version);
        return;
      }
      const data =
        version?.progress !== undefined
          ? version.progress.value
          : (version?.discoverySnapshot ??
            version?.voiceRecords ??
            version?.draftChanges);
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
