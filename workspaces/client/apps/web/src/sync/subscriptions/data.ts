import { watchResourceReads } from "./reads";
import type { ResourceData, ResourceRef, ResourceProgress } from "./contracts";
import {
  selectResourceData,
  recoverResourceData,
  type ResourceDataSelectors,
  type ViewRecovery,
} from "./selectData";
import type { ObserveSubscriptionData } from "./module";
import type { ResourceVersion } from "../resourceEvents";
import type { createSyncClient, TranscriptProjectionPayload } from "../client";
import type { SubscriptionTransport } from "./store";

const viewSelectors: Required<
  Pick<ResourceDataSelectors<ResourceVersion | undefined>, keyof ViewRecovery>
> = {
  processes: (_ref, version) => version?.viewSnapshot?.processes,
  complaint: (_ref, version) => version?.viewSnapshot?.complaint,
  room: (_ref, version) =>
    version?.viewSnapshot?.room ?? version?.viewSnapshot?.legacyMessages,
  "terminal-output": (_ref, version) => version?.viewSnapshot?.terminalOutput,
};
const transcriptSelectors: ResourceDataSelectors<TranscriptProjectionPayload> =
  {
    transcript: (_ref, data) => data ?? undefined,
  };

function hasProgressKind<K extends ResourceProgress["resource"]["kind"]>(
  progress: ResourceProgress | undefined,
  kind: K,
): progress is Extract<ResourceProgress, { resource: { kind: K } }> {
  return progress?.resource.kind === kind;
}
function progressFor<K extends ResourceProgress["resource"]["kind"]>(
  kind: K,
  version?: ResourceVersion,
): Extract<ResourceProgress, { resource: { kind: K } }> | undefined {
  const progress = version?.progress;
  return hasProgressKind(progress, kind) ? progress : undefined;
}
const transientSelectors: ResourceDataSelectors<ResourceVersion | undefined> = {
  "auth-progress": (_ref, version) =>
    progressFor("auth-progress", version)?.value,
  "vault-job-progress": (_ref, version) =>
    progressFor("vault-job-progress", version)?.value,
  "vault-readiness": (_ref, version) =>
    progressFor("vault-readiness", version)?.value,
  "rename-progress": (_ref, version) =>
    progressFor("rename-progress", version)?.value,
  "multi-server": (_ref, version) => version?.discoverySnapshot,
  voice: (_ref, version) => version?.voiceRecords,
  drafts: (_ref, version) => version?.draftChanges,
};

/** Apply inline stream values; only oversized/missing recovery frames may read HTTP. */
export function createDataObserver(
  sync: () => Pick<
    ReturnType<typeof createSyncClient>,
    "subscribeTranscriptProjection"
  >,
  events: SubscriptionTransport,
  observeEntities?: ObserveSubscriptionData,
  recoverView?: ViewRecovery,
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
          accept(selectResourceData(transcriptSelectors, ref, data));
        },
        (error) => {
          if (error !== null) failed(error);
        },
        true,
        ref,
      );
    }
    const viewValue = (version?: ResourceVersion) =>
      selectResourceData(viewSelectors, ref, version);
    if (recoverView && ref.kind in recoverView) {
      let stopped = false;
      const stop = watchResourceReads(
        ref,
        async (version) => {
          const value =
            viewValue(version) ?? (await recoverResourceData(recoverView, ref));
          if (!stopped) accept(value, version);
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
        accept(viewData, version);
        return;
      }
      const data = selectResourceData(transientSelectors, ref, version);
      if (data !== undefined) accept(data);
    };
    const stop = events.watchResourceChanges(ref, transient);
    const stopEntities = observeEntities?.(ref, accept, failed);
    return () => {
      stop();
      stopEntities?.();
    };
  };
}
