import { hasNativeClient } from "../native/client";

const LOOPBACK_HOSTS = ["localhost", "127.0.0.1", "[::1]", "::1"];

/**
 * True when this page runs on the same machine as the server of `owner`. Such
 * a frontend keeps no cache. Only the "local" owner can be on this machine:
 * the desktop shell and a loopback origin (including the isolated view hosts
 * of `.localhost`) reach it. Any other owner, and a page opened from another
 * machine's address, is remote.
 */
export function frontendIsLocal(
  owner: string,
  nativeClient: boolean = hasNativeClient(),
  hostname: string = globalThis.location?.hostname ?? "",
) {
  if (owner !== "local") return false;
  return (
    nativeClient ||
    LOOPBACK_HOSTS.includes(hostname) ||
    hostname.endsWith(".localhost")
  );
}
