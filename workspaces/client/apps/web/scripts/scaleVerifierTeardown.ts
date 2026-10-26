// Keep signal ownership and the event loop alive until the verifier's teardown
// settles. Signals request cancellation of work; they never start another delete.
import { writeSync } from "node:fs";

// Node's conventional exit codes for these process signals.
const SIGNAL_EXIT_CODES = { SIGINT: 130, SIGTERM: 143 };
// The third interrupt is the explicit escape hatch for a stuck teardown.
const FORCE_EXIT_SIGNAL_COUNT = 3;
// Keep pending promises alive even when they have no active I/O, milliseconds.
const KEEP_ALIVE_INTERVAL_MS = 1_000;

export type ScaleVerifierLifecycle = {
  setCleanup(callback: () => Promise<void>): void;
  setRemaining(callback: () => unknown): void;
};
export async function runScaleVerifier<T>(
  run: (lifecycle: ScaleVerifierLifecycle) => Promise<T>,
  { onInterrupt }: { onInterrupt?: (signal: NodeJS.Signals) => void } = {},
) {
  let cleanup = async () => {};
  let cleanupRegistered = false;
  let remaining: () => unknown = () => ({ state: "ownership not established" });
  let signalCount = 0;
  const handlers = new Map<NodeJS.Signals, () => void>();
  const keepAlive = setInterval(() => {}, KEEP_ALIVE_INTERVAL_MS);
  for (const signal of ["SIGINT", "SIGTERM"] as const) {
    const code = SIGNAL_EXIT_CODES[signal];
    const handler = () => {
      signalCount += 1;
      process.exitCode = code;
      if (signalCount >= FORCE_EXIT_SIGNAL_COUNT) {
        // Synchronous output must reach stderr before process.exit truncates I/O.
        writeSync(
          process.stderr.fd,
          `Forced exit after ${signal}; last known remaining ownership (unverified): ${JSON.stringify(remaining())}\n`,
        );
        process.exit(code);
      }
      process.stderr.write(`Received ${signal}; waiting for teardown.\n`);
      if (signalCount === 1) onInterrupt?.(signal);
    };
    handlers.set(signal, handler);
    process.on(signal, handler);
  }
  try {
    return await run({
      setCleanup(callback) {
        if (cleanupRegistered) throw new Error("Teardown already registered.");
        cleanupRegistered = true;
        cleanup = callback;
      },
      setRemaining(callback) {
        remaining = callback;
      },
    });
  } finally {
    try {
      await cleanup();
    } finally {
      clearInterval(keepAlive);
      for (const [signal, handler] of handlers) process.off(signal, handler);
    }
  }
}
