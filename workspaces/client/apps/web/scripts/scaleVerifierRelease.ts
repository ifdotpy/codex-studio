import { closeSync, lstatSync, openSync, writeSync } from "node:fs";
import { isAbsolute } from "node:path";

// Arguments are the same path/token pairs supplied by releaseFifoBarriers;
// the module path occupies argv[1] when Node runs this checked child program.
for (let index = 2; index < process.argv.length; index += 2) {
  const path = process.argv[index];
  const token = process.argv[index + 1];
  if (!path || !token || !isAbsolute(path) || !/^[0-9a-f-]{36}$/i.test(token))
    throw new Error("Unexpected worker identity.");
  if (!lstatSync(path).isFIFO()) throw new Error("Expected an owned FIFO.");
  const fd = openSync(path, "w");
  try {
    writeSync(fd, token + "\n");
  } finally {
    closeSync(fd);
  }
}
