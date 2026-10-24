import {
  watchResourceChanges as defaultWatchResourceChanges,
  watchResourceConnection as defaultWatchResourceConnection,
} from "./events";
import { onResume } from "../resume";
import { ReadRecoveryBudget, retryableReadError } from "../readRetry";
import type {
  ResourceRef,
  ResourceStreamState,
  ResourceVersion,
} from "./events";

type ResourceReadWatcher = (() => void) & { refresh: () => void };
type ReadTrigger = { resourceKey: string; version?: ResourceVersion };

/**
 * Run one targeted read for the initial resource baseline and each later
 * resource revision. Reopened streams can repeat an unchanged baseline, so
 * successful versioned reads are remembered and identical revisions skipped.
 * If a notification arrives during a read, perform one more read after it
 * settles so the latest version cannot be lost.
 */
export function watchResourceReads(
  resources: ResourceRef | readonly ResourceRef[],
  read: (version?: ResourceVersion) => Promise<void>,
  failed: (error: unknown) => void,
  options: {
    waitForBaseline?: boolean;
    events?: {
      watchResourceChanges: typeof defaultWatchResourceChanges;
      watchResourceConnection: typeof defaultWatchResourceConnection;
    };
  } = {},
): ResourceReadWatcher {
  const watchResourceChanges =
    options.events?.watchResourceChanges ?? defaultWatchResourceChanges;
  const watchResourceConnection =
    options.events?.watchResourceConnection ?? defaultWatchResourceConnection;
  const refs = Array.isArray(resources) ? resources : [resources];
  const waitForBaseline = options.waitForBaseline === true;
  let active = true;
  let baselineSeen = !waitForBaseline;
  let fallbackReady = false;
  let reading = false;
  let dirty = false;
  let dirtyTrigger: ReadTrigger | undefined;
  const lastSuccessfulVersions = new Map<string, ResourceVersion>();
  let readingTrigger: ReadTrigger | undefined;
  let scheduled = false;
  let recovery = new ReadRecoveryBudget();
  let retryCount = 0;
  const notifiedVersions = new Map<string, ResourceVersion>();
  let retryTimer: ReturnType<typeof setTimeout> | undefined;

  const canRead = () =>
    (typeof document === "undefined" || !document.hidden) &&
    (typeof navigator === "undefined" || navigator.onLine !== false);
  const clearRetry = () => {
    clearTimeout(retryTimer);
    retryTimer = undefined;
  };
  const sameVersion = (
    left: ResourceVersion | undefined,
    right: ResourceVersion | undefined,
  ) =>
    !!left &&
    !!right &&
    left.epoch === right.epoch &&
    left.revision === right.revision;

  const refresh = (version?: ResourceVersion, resourceKey?: string) => {
    const trigger =
      resourceKey === undefined ? undefined : { resourceKey, version };
    if (version) {
      baselineSeen = true;
      if (
        (trigger &&
          sameVersion(
            version,
            lastSuccessfulVersions.get(trigger.resourceKey),
          )) ||
        (trigger &&
          readingTrigger?.resourceKey === trigger.resourceKey &&
          sameVersion(version, readingTrigger.version))
      )
        return;
    }
    if (recovery.exhaustedError) return;
    if (waitForBaseline && !baselineSeen && !fallbackReady) return;
    dirty = true;
    dirtyTrigger = trigger;
    if (
      !active ||
      reading ||
      scheduled ||
      retryTimer !== undefined ||
      (!canRead() && !fallbackReady)
    )
      return;
    scheduled = true;
    queueMicrotask(() => {
      scheduled = false;
      if (!active || reading || !dirty || (!canRead() && !fallbackReady))
        return;
      reading = true;
      void (async () => {
        try {
          while (active && dirty && (canRead() || fallbackReady)) {
            dirty = false;
            const currentTrigger = dirtyTrigger;
            dirtyTrigger = undefined;
            if (
              currentTrigger?.version &&
              sameVersion(
                currentTrigger.version,
                lastSuccessfulVersions.get(currentTrigger.resourceKey),
              )
            )
              continue;
            readingTrigger = currentTrigger;
            const currentRecovery = recovery;
            try {
              await read(currentTrigger?.version);
              if (currentTrigger?.version)
                lastSuccessfulVersions.set(
                  currentTrigger.resourceKey,
                  currentTrigger.version,
                );
              retryCount = 0;
            } catch (error) {
              if (!active) break;
              // A newer notification owns the next read and its fresh budget.
              if (currentRecovery !== recovery) continue;
              let reportedError = error;
              let delay: number | undefined;
              if (retryableReadError(error)) {
                try {
                  delay = recovery.nextDelay(error, retryCount++);
                } catch (exhausted) {
                  reportedError = exhausted;
                }
              }
              try {
                failed(reportedError);
              } catch {
                // The failure reporter cannot prevent recovery of the read.
              }
              if (!active) break;
              if (currentRecovery !== recovery) continue;
              if (delay !== undefined) {
                dirty = true;
                if (currentTrigger && !dirtyTrigger)
                  dirtyTrigger = currentTrigger;
                if (canRead()) {
                  retryTimer = setTimeout(() => {
                    retryTimer = undefined;
                    refresh(dirtyTrigger?.version, dirtyTrigger?.resourceKey);
                  }, delay);
                }
                break;
              }
              if (recovery.exhaustedError) dirty = false;
            } finally {
              if (readingTrigger === currentTrigger) readingTrigger = undefined;
            }
          }
        } finally {
          reading = false;
        }
      })();
    });
  };

  const stopResume =
    typeof window === "undefined"
      ? () => {}
      : onResume(() => {
          if (dirty) {
            clearRetry();
            refresh(dirtyTrigger?.version, dirtyTrigger?.resourceKey);
          }
        });
  const pause = () => {
    if (!canRead()) clearRetry();
  };
  if (typeof window !== "undefined") window.addEventListener("offline", pause);
  if (typeof document !== "undefined")
    document.addEventListener("visibilitychange", pause);
  const stops = refs.map((resource) => {
    const resourceKey = JSON.stringify(resource);
    return watchResourceChanges(
      resource,
      (version) => {
        const previous = notifiedVersions.get(resourceKey);
        if (
          !version ||
          !previous ||
          version.epoch !== previous.epoch ||
          version.revision > previous.revision
        ) {
          if (recovery.exhaustedError) retryCount = 0;
          recovery = new ReadRecoveryBudget();
          if (version) notifiedVersions.set(resourceKey, version);
        }
        refresh(version, resourceKey);
      },
      { waitForBaseline },
    );
  });
  let fallbackStarted = false;
  let wasLive = false;
  const stopConnection = watchResourceConnection(
    (status, stream: ResourceStreamState) => {
      const reconnected = status === "live" && !wasLive;
      wasLive = status === "live";
      if (reconnected && (dirty || recovery.exhaustedError)) {
        recovery = new ReadRecoveryBudget();
        retryCount = 0;
        clearRetry();
        refresh(dirtyTrigger?.version, dirtyTrigger?.resourceKey);
      }
      if (
        waitForBaseline &&
        !baselineSeen &&
        !fallbackStarted &&
        status !== "schema-mismatch" &&
        (stream === "stopped" || status === "degraded" || status === "offline")
      ) {
        fallbackStarted = true;
        fallbackReady = true;
        refresh();
      }
    },
  );
  const dispose = () => {
    active = false;
    dirty = false;
    clearRetry();
    stopConnection();
    stopResume();
    if (typeof window !== "undefined")
      window.removeEventListener("offline", pause);
    if (typeof document !== "undefined")
      document.removeEventListener("visibilitychange", pause);
    for (const stop of stops) stop();
  };
  return Object.assign(dispose, {
    refresh: () => {
      recovery = new ReadRecoveryBudget();
      retryCount = 0;
      clearRetry();
      refresh();
    },
  });
}
