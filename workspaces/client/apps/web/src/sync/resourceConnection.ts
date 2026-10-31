import { hasNativeClient } from "../native/client";
import type { ReconnectCause } from "./connectionState";

/**
 * Whether the sync transport treats the page as hidden. The desktop app keeps
 * its stream while its window is in the background: a reopen on every window
 * switch replays the initial sync through the native bridge.
 */
export function pageHidden() {
  return document.hidden && !hasNativeClient();
}

export type ResourceConnectionDetail = {
  failure?: ReconnectCause | undefined;
  /** Deadline of the current scheduled retry, in Unix epoch milliseconds. */
  retryAt?: number | undefined;
  attempt?: number | undefined;
};

/** Maximum wait for optional local resume cursors, in milliseconds. */
export const RESOURCE_CURSOR_PREPARATION_TIMEOUT_MS = 5_000;

/** First wait before a stream retry, in milliseconds. */
export const RESOURCE_RETRY_BASE_MS = 500;
/** Longest wait between stream retries, in milliseconds. */
export const RESOURCE_RETRY_MAX_MS = 5 * 60_000;
/** How long a stream must stay up before the retry count starts again, in milliseconds. */
export const RESOURCE_STABLE_MS = 60_000;
/** Longest wait for the answer to a probe of an open stream, in milliseconds. */
export const RESOURCE_PROBE_TIMEOUT_MS = 5_000;

/** Maximum silence after a validated server frame, in milliseconds. */
export const RESOURCE_HEARTBEAT_TIMEOUT_MS = 45_000;
/** Each lifecycle event reaches the transport once. */
export function onResourceConnectionLifecycle(actions: {
  resume: () => void;
  online: () => void;
  offline: () => void;
  pause: () => void;
  /** The kind or quality of the network changed while the device stayed online. */
  change: () => void;
}) {
  const visibility = () => {
    if (pageHidden()) actions.pause();
    else actions.resume();
  };
  window.addEventListener("pageshow", actions.resume);
  window.addEventListener("offline", actions.offline);
  window.addEventListener("online", actions.online);
  document.addEventListener("visibilitychange", visibility);
  const connection = (
    navigator as {
      connection?: Pick<
        EventTarget,
        "addEventListener" | "removeEventListener"
      >;
    }
  ).connection;
  connection?.addEventListener("change", actions.change);
  return () => {
    connection?.removeEventListener("change", actions.change);
    window.removeEventListener("pageshow", actions.resume);
    window.removeEventListener("offline", actions.offline);
    window.removeEventListener("online", actions.online);
    document.removeEventListener("visibilitychange", visibility);
  };
}
