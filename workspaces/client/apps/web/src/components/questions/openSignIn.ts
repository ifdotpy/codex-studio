import type { SignInForward } from "../../desktop";

/** True when this window can hold the redirect port of a sign-in that another server
 * approved: the desktop asks this Mac's Studio server to hold it. */
export function canForwardSignIn(): boolean {
  return window.codexDesktop?.signInForward === true;
}

/** Open a sign-in link that the person approved in a request card
 * (docs/vm-browser-bridge.md). The desktop brings the browser to the front; other links
 * keep opening in the background. A plain browser window opens a new tab. With `forward`,
 * the command waits on another computer: this Mac first holds its redirect port. */
export async function openSignInPage(
  url: string,
  forward?: SignInForward | null,
): Promise<void> {
  const parsed = new URL(url);
  if (!["http:", "https:"].includes(parsed.protocol))
    throw new Error("Only HTTP and HTTPS links are supported.");
  const desktop = window.codexDesktop;
  if (forward) {
    if (!desktop?.openSignIn || !canForwardSignIn())
      throw new Error(
        "Update LAYR Studio on this Mac to sign in to a command on another computer.",
      );
    return desktop.openSignIn({ url: parsed.href, forward });
  }
  if (desktop?.openSignIn) return desktop.openSignIn(parsed.href);
  if (desktop) return desktop.openExternal(parsed.href);
  // "noopener" makes window.open return null, so detach the opener instead.
  const opened = window.open(parsed.href, "_blank");
  if (!opened)
    throw new Error(`Allow pop-ups, or open this link: ${parsed.href}`);
  opened.opener = null;
}
