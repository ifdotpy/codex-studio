import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const grandchild = spawn(
  process.execPath,
  [fileURLToPath(new URL("./fixture-idle.child.ts", import.meta.url))],
  { stdio: "ignore" },
);
if (process.argv[2] === "orphan") grandchild.unref();
process.stdout.write(String(grandchild.pid));
if (process.argv[2] !== "orphan") setInterval(() => {}, 1000);
