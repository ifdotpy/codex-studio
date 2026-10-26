/** Resolve the current element after React updates the visible view. */
export function focusAfterRender(
  selector: string | null,
  fallback?: { readonly current: HTMLElement | null },
) {
  requestAnimationFrame(() => {
    const selected = selector
      ? document.querySelector<HTMLElement>(selector)
      : null;
    (selected || fallback?.current)?.focus();
  });
}
