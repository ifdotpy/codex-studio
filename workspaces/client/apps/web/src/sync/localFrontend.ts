import { hasNativeClient } from "../native/client";

const LOOPBACK_HOSTS = ["localhost", "127.0.0.1", "[::1]", "::1"];
/** The isolated view host of a server (`serverFrameHost`), which resolves to loopback. */
const ISOLATED_VIEW_HOST = /^studio-[a-z2-7.]+\.localhost$/;

/**
 * True when this page runs on the same machine as the server of `owner`. Such
 * a frontend keeps no cache. Only the "local" owner can be on this machine:
 * the desktop shell and a loopback origin (including the isolated view hosts
 * `studio-<id>.localhost`) reach it. Any other owner, and a page opened from another
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
    ISOLATED_VIEW_HOST.test(hostname)
  );
}
