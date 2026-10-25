import { existsSync, watch } from "node:fs";
import path from "node:path";

// Maximum wait for a scripted bridge fixture signal, in milliseconds.
export const HANG_GUARD_MS = 10_000;

// Observe before inspecting so creation, rename and append cannot fall between
// the condition check and subscription. The timeout is only a failure guard.
export function waitForFile(
  file,
  { signal, matches = () => true, absent = false } = {},
) {
  return new Promise((resolve, reject) => {
    const controller = new AbortController();
    const deadline = signal
      ? undefined
      : setTimeout(
          () => controller.abort(new Error(`File signal missing: ${file}`)),
          HANG_GUARD_MS,
        );
    signal ??= controller.signal;
    const watcher = watch(path.dirname(file), check);
    const cleanup = () => {
      watcher.close();
      clearTimeout(deadline);
      signal.removeEventListener("abort", abort);
    };
    function abort() {
      cleanup();
      reject(signal.reason);
    }
    function check() {
      try {
        if (existsSync(file) === absent || !matches()) return;
        cleanup();
        resolve();
      } catch (error) {
        cleanup();
        reject(error);
      }
    }
    watcher.on("error", (error) => {
      cleanup();
      reject(error);
    });
    signal.addEventListener("abort", abort, { once: true });
    if (signal.aborted) abort();
    else check();
  });
}
