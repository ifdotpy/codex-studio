import { hasNativeClient } from "./client";
import { useNativeDrafts } from "./useNativeDrafts";
import { useUnavailableDrafts } from "./unavailableDrafts";

/**
 * The core stores the drafts of the local server only. Every other view
 * has no drafts until a transport reaches its own core.
 */
export const draftsForServer = (localServer: boolean) =>
  localServer && hasNativeClient() ? useNativeDrafts : useUnavailableDrafts;
