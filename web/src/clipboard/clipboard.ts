// The desktop window denies the async Clipboard API permission, and some
// browsers reject it without focus. The legacy copy command only needs the
// user's click, so it is the fallback for every copy button.
export async function copyText(text: string): Promise<void> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return;
    }
  } catch {
    // Fall through to the copy command.
  }
  const active = document.activeElement as HTMLElement | null;
  const selection = document.getSelection();
  const ranges = selection
    ? Array.from({ length: selection.rangeCount }, (_, i) =>
        selection.getRangeAt(i),
      )
    : [];
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.top = "-1000px";
  area.style.opacity = "0";
  document.body.appendChild(area);
  area.select();
  area.setSelectionRange(0, text.length);
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } finally {
    area.remove();
    selection?.removeAllRanges();
    ranges.forEach((range) => selection?.addRange(range));
    active?.focus?.({ preventScroll: true });
  }
  if (!copied) throw new Error("The browser blocked clipboard access");
}
