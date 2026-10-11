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

/**
 * Run one targeted read for the initial resource baseline and each later
 * notification. If a notification arrives during a read, perform one more
 * read after it settles so the latest version cannot be lost.
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
  let dirtyVersion: ResourceVersion | undefined;
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

  const refresh = (version?: ResourceVersion) => {
    if (version) baselineSeen = true;
    if (waitForBaseline && !baselineSeen && !fallbackReady) return;
    dirty = true;
    if (version) dirtyVersion = version;
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
            const version = dirtyVersion;
            dirtyVersion = undefined;
            try {
              await read(version);
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
                if (canRead()) {
                  retryTimer = setTimeout(() => {
                    retryTimer = undefined;
                    refresh();
                  }, readRetryDelay(retryCount++));
                }
                break;
              }
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
            refresh();
          }
        });
  const pause = () => {
    if (!canRead()) clearRetry();
  };
  if (typeof window !== "undefined") window.addEventListener("offline", pause);
  if (typeof document !== "undefined")
    document.addEventListener("visibilitychange", pause);
  const stops = refs.map((resource) =>
    watchResourceChanges(resource, refresh, { waitForBaseline }),
  );
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
