import { hasNativeClient } from "./native/client";
import { openNativeLink } from "./native/externalLinks";

export type { DesktopBridge, SignInForward } from "../../desktop/api.ts";

if (window.codexDesktop?.platform === "darwin") {
  document.documentElement.classList.add("desktop-inset-titlebar");
}

// Preview frames cannot access this main-frame bridge.
document.addEventListener("click", (event) => {
  if (
    (!window.codexDesktop && !hasNativeClient()) ||
    event.defaultPrevented ||
    event.button !== 0
  )
    return;
  const link = (event.target as Element)?.closest?.("a[href]");
  if (!(link instanceof HTMLAnchorElement) || link.hasAttribute("download"))
    return;
  const url = new URL(link.href, location.href);
  if (
    !["http:", "https:"].includes(url.protocol) ||
    url.origin === location.origin
  )
    return;
  event.preventDefault();
  const opened = window.codexDesktop
    ? window.codexDesktop.openExternal(url.href)
    : openNativeLink(url.href);
  void opened.catch((error) => {
    window.dispatchEvent(
      new CustomEvent("desktop-error", {
        detail: String(error.message || error),
      }),
    );
  });
});
