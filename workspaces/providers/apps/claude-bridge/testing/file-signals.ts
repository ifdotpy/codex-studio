import { existsSync, watch } from "node:fs";
import path from "node:path";
import { scheduler } from "node:timers/promises";

// Maximum wait for a scripted bridge fixture signal, in milliseconds.
export const HANG_GUARD_MS = 10_000;

// Observe before inspecting so creation, rename and append cannot fall between
// the condition check and subscription. The timeout is only a failure guard.
// Directory events can miss startup changes. watchFile can also take its first
// stat sample after creation and report no change. Recheck the condition in real
// time; scheduler.wait is outside the scripted child's mocked timer APIs.
const CONDITION_POLL_MS = 20;
export function waitForFile(
  file: string,
  {
    signal,
    matches = () => true,
    absent = false,
    childIpc = false,
  }: {
    signal?: AbortSignal;
    matches?: () => boolean;
    absent?: boolean;
    childIpc?: boolean;
  } = {},
) {
  return new Promise<void>((resolve, reject) => {
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
      signal!.removeEventListener("abort", abort);
      process.off("message", notified);
      controller.abort();
    };
    function abort() {
      cleanup();
      reject(signal!.reason);
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
    function notified(message: { fixtureFile?: string }) {
      if (message?.fixtureFile === file) check();
    }
    async function poll() {
      while (!controller.signal.aborted) {
        await scheduler.wait(CONDITION_POLL_MS, { signal: controller.signal });
        check();
      }
    }
    // Only scripted children opt in: test workers also have process.send, but
    // their IPC reference belongs to the test runner. A publisher notification
    // handles coalesced filesystem events; the file remains the durable condition.
    if (childIpc && typeof process.send === "function") {
      process.on("message", notified);
      process.channel?.unref();
    }
    watcher.on("error", (error) => {
      cleanup();
      reject(error);
    });
    signal.addEventListener("abort", abort, { once: true });
    if (signal.aborted) abort();
    else check();
    void poll().catch((error: unknown) => {
      if (controller.signal.aborted) return;
      cleanup();
      reject(error);
    });
  });
}
