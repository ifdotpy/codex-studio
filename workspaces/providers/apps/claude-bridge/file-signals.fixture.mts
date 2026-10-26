import { once } from "node:events";
import { waitForFile } from "./testing/file-signals.ts";

async function run() {
  const file = process.argv[2];
  if (!file) throw new Error("File signal fixture needs a path");
  const reply = once(process, "message");
  await waitForFile(file);
  process.send?.("ready");
  const [message] = await reply;
  process.send?.(message);
  process.disconnect?.();
}
run().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});
