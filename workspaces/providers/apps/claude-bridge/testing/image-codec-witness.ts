// Run from a deployed production bridge, including read-only Linux containers.
// No SDK query, test framework, host command, or writable bridge directory is needed.
import assert from "node:assert/strict";
import sharp from "sharp";
import {
  prepareClaudeImage,
  MAX_IMAGE_SIDE,
  MAX_IMAGE_BYTES,
  MAX_IMAGE_BASE64_BYTES,
} from "../images.ts";

let cases = 0;
for (const format of ["png", "jpeg", "gif", "webp"]) {
  for (const side of [MAX_IMAGE_SIDE, MAX_IMAGE_SIDE + 1]) {
    const original = await sharp({
      create: { width: side, height: 2, channels: 4, background: "#2080c080" },
    })
      [format]()
      .toBuffer();
    const image = await prepareClaudeImage(original);
    const actual = await sharp(image.bytes).metadata();
    assert.equal(actual.width, MAX_IMAGE_SIDE);
    assert.ok(actual.height >= 1);
    assert.equal(
      image.mediaType,
      "image/" + (side > MAX_IMAGE_SIDE && format === "gif" ? "png" : format),
    );
    assert.ok(image.bytes.toString("base64").length <= MAX_IMAGE_BASE64_BYTES);
    if (side === MAX_IMAGE_SIDE) assert.deepEqual(image.bytes, original);
    cases++;
  }
  const original = await sharp({
    create: { width: 32, height: 16, channels: 3, background: "red" },
  })
    [format]()
    .toBuffer();
  const padded = Buffer.concat([
    original,
    Buffer.alloc(MAX_IMAGE_BYTES + 1 - original.length),
  ]);
  const image = await prepareClaudeImage(padded);
  assert.ok(image.bytes.length <= MAX_IMAGE_BYTES);
  assert.ok(image.bytes.toString("base64").length <= MAX_IMAGE_BASE64_BYTES);
  assert.equal(
    image.mediaType,
    "image/" + (await sharp(image.bytes).metadata()).format,
  );
  cases++;
}
for (const bytes of [Buffer.alloc(0), Buffer.from("damaged")]) {
  await assert.rejects(prepareClaudeImage(bytes), /Cannot send image/);
  cases++;
}
console.log(
  JSON.stringify({
    platform: process.platform,
    arch: process.arch,
    sharp: sharp.versions.sharp,
    cases,
    result: "passed",
  }),
);
