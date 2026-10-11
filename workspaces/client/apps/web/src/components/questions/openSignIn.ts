/** Open a sign-in link that the person approved in a request card
 * (docs/vm-browser-bridge.md). The desktop brings the browser to the front; other links
 * keep opening in the background. A plain browser window opens a new tab. */
export async function openSignInPage(url: string): Promise<void> {
  const parsed = new URL(url);
  if (!["http:", "https:"].includes(parsed.protocol))
    throw new Error("Only HTTP and HTTPS links are supported.");
  const desktop = window.codexDesktop;
  if (desktop?.openSignIn) return desktop.openSignIn(parsed.href);
  if (desktop) return desktop.openExternal(parsed.href);
  // "noopener" makes window.open return null, so detach the opener instead.
  const opened = window.open(parsed.href, "_blank");
  if (!opened)
    throw new Error(`Allow pop-ups, or open this link: ${parsed.href}`);
  opened.opener = null;
}
