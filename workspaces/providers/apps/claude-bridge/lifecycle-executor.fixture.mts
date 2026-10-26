import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).on("line", (line: string) => {
  const message: unknown = JSON.parse(line);
  if (
    message &&
    typeof message === "object" &&
    "id" in message &&
    message.id !== undefined
  )
    process.stdout.write(JSON.stringify({ id: message.id }) + "\n");
});
