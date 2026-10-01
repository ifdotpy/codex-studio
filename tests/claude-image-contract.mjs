import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import zlib from "node:zlib";
import { claudeImage, MAX_IMAGE_SIDE } from "../scripts/claude_bridge/images.mjs";

// Write an uncompressed RGB PNG with noise so its size stays realistic.
function png(width, height) {
  const crcTable = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (buf) => {
    let c = 0xffffffff;
    for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type, data) => {
    const out = Buffer.alloc(12 + data.length);
    out.writeUInt32BE(data.length, 0);
    out.write(type, 4, "ascii");
    data.copy(out, 8);
    out.writeUInt32BE(crc(out.subarray(4, 8 + data.length)), 8 + data.length);
    return out;
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header[8] = 8;
  header[9] = 2;
  const rows = Buffer.alloc((width * 3 + 1) * height);
  for (let i = 0; i < rows.length; i++) rows[i] = (i * 2654435761) >>> 24;
  for (let y = 0; y < height; y++) rows[y * (width * 3 + 1)] = 0;
  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk("IHDR", header),
    chunk("IDAT", zlib.deflateSync(rows, { level: 1 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}
const side = (file) => {
  const out = execFileSync("sips", ["-g", "pixelWidth", "-g", "pixelHeight", file], { encoding: "utf8" });
  return [Number(/pixelWidth:\s*(\d+)/.exec(out)[1]), Number(/pixelHeight:\s*(\d+)/.exec(out)[1])];
};

const dir = await fs.mkdtemp(path.join(os.tmpdir(), "claude-image-contract-"));
try {
  const large = path.join(dir, "large.png");
  await fs.writeFile(large, png(3024, 1534));
  const before = await fs.readFile(large);
  const result = await claudeImage(large, "image/png");
  const out = path.join(dir, "out" + (result.mediaType === "image/png" ? ".png" : ".jpg"));
  await fs.writeFile(out, result.bytes);
  const [w, h] = side(out);
  assert.equal(Math.max(w, h), MAX_IMAGE_SIDE);
  assert.ok(result.bytes.length <= 3_750_000);
  assert.deepEqual(await fs.readFile(large), before, "The source file stays unchanged");

  const small = path.join(dir, "small.png");
  await fs.writeFile(small, png(800, 600));
  const kept = await claudeImage(small, "image/png");
  assert.deepEqual(kept.bytes, await fs.readFile(small));
  assert.equal(kept.mediaType, "image/png");
  console.log("PASS claude image: large screenshot scaled to", w, "x", h, result.mediaType, result.bytes.length, "bytes; small image unchanged");
} finally {
  await fs.rm(dir, { recursive: true, force: true });
}
