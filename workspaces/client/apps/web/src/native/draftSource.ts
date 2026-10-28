import type { CoreClient } from "./client";
import { nativeDraftsHook } from "./useNativeDrafts";
import { useUnavailableDrafts } from "./unavailableDrafts";

/**
 * The core of each server stores its drafts. A page without a client core
 * has no drafts.
 */
export const draftsForServer = (core: CoreClient | undefined) =>
  core ? nativeDraftsHook(core) : useUnavailableDrafts;
