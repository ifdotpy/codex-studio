import { existsSync, watch } from "node:fs";
import path from "node:path";

// Maximum wait for a scripted bridge fixture signal, in milliseconds.
export const HANG_GUARD_MS = 10_000;

// Observe before inspecting so creation, rename and append cannot fall between
// the condition check and subscription. The timeout is only a failure guard.
export function waitForFile(
  file: string,
  {
    signal: suppliedSignal,
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
    const signal = suppliedSignal ?? controller.signal;
    const deadline = suppliedSignal
      ? undefined
      : setTimeout(
          () => controller.abort(new Error(`File signal missing: ${file}`)),
          HANG_GUARD_MS,
        );
    const watcher = watch(path.dirname(file), check);
    const cleanup = () => {
      watcher.close();
      clearTimeout(deadline);
      signal.removeEventListener("abort", abort);
      process.off("message", notified);
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
    function notified(message: unknown) {
      if (
        message &&
        typeof message === "object" &&
        "fixtureFile" in message &&
        message.fixtureFile === file
      )
        check();
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
  });
}
