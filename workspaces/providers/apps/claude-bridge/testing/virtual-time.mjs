// Loaded only by scripted bridge children that opt into controlled time.
import { mock } from "node:test";
mock.timers.enable({ apis: ["Date", "setTimeout", "setInterval"] });
process.on("message", ({ advance }) => {
  if (typeof advance !== "number") return;
  mock.timers.tick(advance);
  // Finish the timer callbacks' promise continuations before acknowledging.
  setImmediate(() => process.send("advanced"));
});
