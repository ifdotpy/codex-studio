import type { components } from "../generated/api";
import { watchResourceChanges } from "../sync/resourceEvents";
import { onResume } from "../sync/resume";
import { readRetryDelay, retryableReadError } from "../sync/readRetry";
import type { ResourceVersion } from "../sync/resourceEvents";

type ResourceRef = components["schemas"]["ResourceRef"];
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
): ResourceReadWatcher {
  const refs = Array.isArray(resources) ? resources : [resources];
  let active = true;
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
    dirty = true;
    if (version) dirtyVersion = version;
    if (
      !active ||
      reading ||
      scheduled ||
      retryTimer !== undefined ||
      !canRead()
    )
      return;
    scheduled = true;
    queueMicrotask(() => {
      scheduled = false;
      if (!active || reading || !dirty || !canRead()) return;
      reading = true;
      void (async () => {
        try {
          while (active && dirty && canRead()) {
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
  const stops = refs.map((resource) => watchResourceChanges(resource, refresh));
  const dispose = () => {
    active = false;
    dirty = false;
    clearRetry();
    stopResume();
    if (typeof window !== "undefined")
      window.removeEventListener("offline", pause);
    if (typeof document !== "undefined")
      document.removeEventListener("visibilitychange", pause);
    for (const stop of stops) stop();
  };
  return Object.assign(dispose, { refresh });
}
