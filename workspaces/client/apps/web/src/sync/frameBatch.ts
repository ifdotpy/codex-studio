/** Coalesce UI publication within a frame. The timer also serves hidden tabs. */
export function createFrameBatch(publish: () => void) {
  let pending = false;
  let frame: number | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const cancel = () => {
    pending = false;
    if (frame !== undefined) cancelAnimationFrame(frame);
    clearTimeout(timer);
    frame = undefined;
    timer = undefined;
  };
  const flush = () => {
    if (!pending) return;
    cancel();
    publish();
  };
  return {
    schedule() {
      if (pending) return;
      pending = true;
      if (
        typeof requestAnimationFrame === "function" &&
        !globalThis.document?.hidden
      )
        frame = requestAnimationFrame(flush);
      timer = setTimeout(flush, 50);
    },
    cancel,
  };
}
