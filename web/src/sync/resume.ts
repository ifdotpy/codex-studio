/** Resume after a network change, a PWA return, or restoration from page history. */
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setTimeout> | undefined;
const resume = () => {
  if (document.hidden || navigator.onLine === false || timer !== undefined)
    return;
  // Mobile browsers can report pageshow, visibility and online for one return.
  timer = setTimeout(() => {
    timer = undefined;
    if (!document.hidden && navigator.onLine !== false)
      for (const action of [...listeners]) action();
  }, 50);
};
export function onResume(action: () => void) {
  if (!listeners.size) {
    window.addEventListener("online", resume);
    window.addEventListener("pageshow", resume);
    document.addEventListener("visibilitychange", resume);
  }
  listeners.add(action);
  return () => {
    listeners.delete(action);
    if (!listeners.size) {
      clearTimeout(timer);
      timer = undefined;
      window.removeEventListener("online", resume);
      window.removeEventListener("pageshow", resume);
      document.removeEventListener("visibilitychange", resume);
    }
  };
}
