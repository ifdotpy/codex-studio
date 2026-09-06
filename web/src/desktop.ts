export interface DesktopBridge {
  platform: string;
  pickDirectory(): Promise<string | null>;
  pickFiles(): Promise<
    { name: string; path: string; mime: string; data: string }[]
  >;
  revealPath(path: string): Promise<void>;
  openExternal(url: string): Promise<void>;
  setNotifications(enabled: boolean): Promise<boolean>;
  notify(value: { title: string; body: string }): Promise<boolean>;
}
declare global {
  interface Window {
    codexDesktop?: DesktopBridge;
  }
}

// Preview frames cannot access this main-frame bridge.
document.addEventListener("click", (event) => {
  if (!window.codexDesktop || event.defaultPrevented || event.button !== 0)
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
  void window.codexDesktop.openExternal(url.href).catch((error) => {
    window.dispatchEvent(
      new CustomEvent("desktop-error", {
        detail: String(error.message || error),
      }),
    );
  });
});
