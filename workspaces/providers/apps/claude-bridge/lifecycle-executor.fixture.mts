import { createInterface } from "node:readline";
createInterface({ input: process.stdin }).on("line", (line: string) => {
  const message: { id?: unknown } = JSON.parse(line);
  if (message.id !== undefined)
    process.stdout.write(JSON.stringify({ id: message.id }) + "\n");
});
