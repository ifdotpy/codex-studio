// Loaded only by scripted bridge children that opt into controlled time.
import { mock } from "node:test";
mock.timers.enable({ apis: ["Date", "setTimeout", "setInterval"] });
// Mocked timers hold no event-loop handle, so the IPC channel that drives this
// clock keeps the child alive until its virtual timers have run. File-signal
// waits unref the channel for children without a controlled clock; restore it.
const keepClockChannel = () => process.channel?.ref();
keepClockChannel();
process.on("message", (message: unknown) => {
  if (!message || typeof message !== "object" || !("advance" in message))
    return;
  const { advance } = message;
  if (typeof advance !== "number") return;
  keepClockChannel();
  mock.timers.tick(advance);
  // Finish the timer callbacks' promise continuations before acknowledging.
  setImmediate(() => process.send?.("advanced"));
});
