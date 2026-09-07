/** Resume after a network change, a PWA return, or restoration from page history. */
export function onResume(action: () => void) {
  const visible = () => {
    if (!document.hidden) action();
  };
  window.addEventListener("online", action);
  window.addEventListener("pageshow", visible);
  document.addEventListener("visibilitychange", visible);
  return () => {
    window.removeEventListener("online", action);
    window.removeEventListener("pageshow", visible);
    document.removeEventListener("visibilitychange", visible);
  };
}
