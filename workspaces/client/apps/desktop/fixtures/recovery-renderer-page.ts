import type {} from "../api.ts";
export function heading() {
  return document.querySelector("h1")!.textContent;
}
export function fallbackDenied() {
  return window.codexDesktop!.getBackendUpdate!().then(
    () => false,
    (error: unknown) => (error as Error).message.includes("restricted"),
  );
}
export function reloadWorkspace() {
  document.querySelector<HTMLElement>("#reload-workspace")!.click();
}
export function backendUpdateType() {
  return typeof window.codexDesktop!.getBackendUpdate;
}
