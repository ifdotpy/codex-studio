import type {} from "../api.ts";
export function heading() {
  return document.querySelector("h1")?.textContent;
}
export function fallbackDenied() {
  const desktop = window.codexDesktop;
  if (!desktop?.getBackendUpdate) throw new Error("Desktop bridge unavailable");
  return desktop.getBackendUpdate().then(
    () => false,
    (error: unknown) =>
      error instanceof Error && error.message.includes("restricted"),
  );
}
export function reloadWorkspace() {
  const link = document.querySelector("#reload-workspace");
  if (!(link instanceof HTMLElement))
    throw new Error("Reload link unavailable");
  link.click();
}
export function backendUpdateType() {
  return typeof window.codexDesktop?.getBackendUpdate;
}
