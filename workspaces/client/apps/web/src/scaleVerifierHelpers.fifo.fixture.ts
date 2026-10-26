import assert from "node:assert/strict";
import { openSync, readSync } from "node:fs";

const path = process.argv[2];
assert.ok(path);
const fd = openSync(path, "r+");
process.stdout.write("READY");
const buffer = Buffer.alloc(64);
const size = readSync(fd, buffer, 0, buffer.length, null);
if (buffer.subarray(0, size).toString().trim() !== process.argv[3])
  process.exit(4);
process.stdout.write("released");
