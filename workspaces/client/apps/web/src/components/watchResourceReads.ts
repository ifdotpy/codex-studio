import {
  watchResourceChanges,
  watchResourceConnection,
} from "../sync/resourceEvents";
import { onResume } from "../sync/resume";
import { readRetryDelay, retryableReadError } from "../sync/readRetry";
import type {
  ResourceRef,
  ResourceStreamState,
  ResourceVersion,
} from "../sync/resourceEvents";

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
  options: { waitForBaseline?: boolean } = {},
): ResourceReadWatcher {
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
  let retryCount = 0;
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
              try {
                failed(error);
              } catch {
                // The failure reporter cannot prevent recovery of the read.
              }
              if (!active) break;
              if (retryableReadError(error)) {
                dirty = true;
                if (currentTrigger && !dirtyTrigger)
                  dirtyTrigger = currentTrigger;
                if (canRead()) {
                  retryTimer = setTimeout(() => {
                    retryTimer = undefined;
                    refresh(dirtyTrigger?.version, dirtyTrigger?.resourceKey);
                  }, readRetryDelay(retryCount++));
                }
                break;
              }
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
      (version) => refresh(version, resourceKey),
      { waitForBaseline },
    );
  });
  let fallbackStarted = false;
  const stopConnection = waitForBaseline
    ? watchResourceConnection((status, stream: ResourceStreamState) => {
        if (
          !baselineSeen &&
          !fallbackStarted &&
          status !== "schema-mismatch" &&
          (stream === "stopped" ||
            status === "degraded" ||
            status === "offline")
        ) {
          fallbackStarted = true;
          fallbackReady = true;
          refresh();
        }
      })
    : () => {};
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
  return Object.assign(dispose, { refresh });
}
