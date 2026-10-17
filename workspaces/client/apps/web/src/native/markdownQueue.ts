// Keep pure rendering work below the frame (32) and native (64) request limits.
// Writes never enter this queue. Started requests retain their slot until reply.
let active = 0;
const waiting = new Set<() => void>();
const LIMIT = 4;

export function queueMarkdown<T>(
  work: () => Promise<T>,
  signal?: AbortSignal,
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const cancel = () => {
      waiting.delete(start);
      signal?.removeEventListener("abort", cancel);
      reject(new DOMException("Markdown rendering cancelled.", "AbortError"));
    };
    const start = () => {
      waiting.delete(start);
      signal?.removeEventListener("abort", cancel);
      if (signal?.aborted) {
        cancel();
        return;
      }
      active++;
      void Promise.resolve()
        .then(work)
        .then(resolve, reject)
        .finally(() => {
          active--;
          while (active < LIMIT && waiting.size)
            waiting.values().next().value!();
        });
    };
    if (signal?.aborted) {
      cancel();
      return;
    }
    signal?.addEventListener("abort", cancel, { once: true });
    if (active < LIMIT) start();
    else waiting.add(start);
  });
}
