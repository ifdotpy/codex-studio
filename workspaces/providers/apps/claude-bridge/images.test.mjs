import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import sharp from "sharp";
import { test } from "vitest";
import {
  claudeImage,
  prepareClaudeImage,
  MAX_IMAGE_SIDE,
  MAX_IMAGE_BYTES,
  MAX_IMAGE_BASE64_BYTES,
  IMAGE_BYTE_BUDGET,
} from "./images.mjs";

// Real codecs produce the fixtures. No host image command is installed or used.
async function fixture(format, width = 32, height = 16) {
  return sharp({
    create: { width, height, channels: 4, background: "#2080c080" },
  })
    [format]()
    .toBuffer();
}

async function withImage(bytes, format, run) {
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), "claude-images-"));
  const file = path.join(directory, "input." + format);
  try {
    await fs.writeFile(file, bytes);
    return await run(file);
  } finally {
    await fs.rm(directory, { recursive: true, force: true });
  }
}

for (const format of ["png", "jpeg", "gif", "webp"]) {
  test(`${format}: small image bytes remain unchanged`, async () => {
    const bytes = await fixture(format);
    await withImage(bytes, format, async (file) => {
      const image = await claudeImage(file, "image/" + format);
      assert.deepEqual(image, { bytes, mediaType: "image/" + format });
    });
  });

  test(`${format}: dimensions above the API limit are reduced`, async () => {
    const bytes = await fixture(format, 2200, 1100);
    await withImage(bytes, format, async (file) => {
      const image = await claudeImage(file, "image/" + format);
      const actual = await sharp(image.bytes).metadata();
      assert.equal(actual.width, 2000);
      assert.equal(actual.height, 1000);
      if (format === "png") assert.equal(actual.hasAlpha, true);
      assert.equal(
        image.mediaType,
        "image/" + (format === "gif" ? "png" : format),
      );
      assert.deepEqual(await fs.readFile(file), bytes);
    });
  });
}

for (const bytes of [Buffer.alloc(0), Buffer.from("damaged image")]) {
  test(`refuses an unreadable image (${bytes.length} bytes)`, async () => {
    await withImage(bytes, "png", async (file) => {
      await assert.rejects(claudeImage(file, "image/png"), /Cannot send image/);
    });
  });
}

for (const side of [MAX_IMAGE_SIDE - 1, MAX_IMAGE_SIDE, MAX_IMAGE_SIDE + 1]) {
  for (const portrait of [false, true]) {
    test(`dimension edge ${side}, portrait=${portrait}`, async () => {
      const bytes = await fixture(
        "png",
        portrait ? 2 : side,
        portrait ? side : 2,
      );
      const image = await prepareClaudeImage(bytes);
      const actual = await sharp(image.bytes).metadata();
      assert.equal(
        Math.max(actual.width, actual.height),
        Math.min(side, MAX_IMAGE_SIDE),
      );
      assert.ok(Math.min(actual.width, actual.height) >= 1);
      if (side <= MAX_IMAGE_SIDE) assert.deepEqual(image.bytes, bytes);
    });
  }
}

for (const format of ["png", "jpeg", "gif", "webp"]) {
  for (const length of [
    IMAGE_BYTE_BUDGET,
    MAX_IMAGE_BYTES,
    MAX_IMAGE_BYTES + 1,
  ]) {
    test(`${format}: byte edge ${length}`, async () => {
      const original = await fixture(format);
      // Decoders permit trailing data. This makes exact byte edges deterministic.
      const bytes = Buffer.concat([
        original,
        Buffer.alloc(length - original.length),
      ]);
      const image = await prepareClaudeImage(bytes);
      const actual = await sharp(image.bytes).metadata();
      assert.ok(image.bytes.length <= MAX_IMAGE_BYTES);
      assert.ok(
        image.bytes.toString("base64").length <= MAX_IMAGE_BASE64_BYTES,
      );
      assert.ok(
        actual.width <= MAX_IMAGE_SIDE && actual.height <= MAX_IMAGE_SIDE,
      );
      assert.equal(image.mediaType, "image/" + actual.format);
      if (length === IMAGE_BYTE_BUDGET) assert.deepEqual(image.bytes, bytes);
    });
  }
}

test("reads the actual format instead of trusting the extension", async () => {
  await withImage(await fixture("webp"), "png", async (file) => {
    assert.equal(
      (await claudeImage(file, "image/png")).mediaType,
      "image/webp",
    );
  });
});

test("refuses unsupported formats and unreadable files", async () => {
  const bytes = await sharp({
    create: { width: 1, height: 1, channels: 3, background: "red" },
  })
    .tiff()
    .toBuffer();
  await assert.rejects(
    prepareClaudeImage(bytes),
    /Cannot send image.*format is unsupported/,
  );
  await withImage(bytes, "tiff", async (file) => {
    await fs.rm(file);
    await assert.rejects(
      claudeImage(file),
      /input.tiff: Cannot send image.*cannot be read/,
    );
  });
});

test("refuses all image input when the native codec cannot load", async () => {
  const unavailable = async () => {
    throw new Error("missing native binary");
  };
  for (const length of [1, MAX_IMAGE_BYTES + 1])
    await assert.rejects(
      prepareClaudeImage(Buffer.alloc(length), unavailable),
      /Cannot send image/,
    );
});

// Simulate encoders that cannot meet the target. Real codec fixtures above verify
// pixels and MIME types; this seam verifies pass bounds and the final refusal.
function codecFixture({
  format = "jpeg",
  width = 2500,
  height = 1250,
  size = MAX_IMAGE_BYTES + 1,
  encode,
}) {
  const bytes = Buffer.alloc(size);
  const metadata = new WeakMap([[bytes, { format, width, height }]]);
  const calls = [];
  const codec = (input) => {
    let info = { ...metadata.get(input) },
      options = {};
    const chain = {
      metadata: async () => ({ ...info }),
      resize(w, h) {
        info.width = w;
        info.height = h;
        return chain;
      },
      jpeg(value) {
        info.format = "jpeg";
        options = value;
        return chain;
      },
      png(value = {}) {
        info.format = "png";
        options = value;
        return chain;
      },
      async toBuffer() {
        const call = { ...info, ...options };
        calls.push(call);
        const result = encode(call, calls.length);
        const output = Buffer.alloc(result.size);
        metadata.set(output, { ...info, ...result.metadata });
        return output;
      },
    };
    return chain;
  };
  return { bytes, calls, loader: async () => codec };
}

test("uses four quality passes, final shrink and five budget passes, then refuses", async () => {
  const fixture = codecFixture({
    encode: () => ({ size: MAX_IMAGE_BYTES + 1 }),
  });
  await assert.rejects(
    prepareClaudeImage(fixture.bytes, fixture.loader),
    /Cannot send image.*limits.*smaller image/,
  );
  assert.deepEqual(
    fixture.calls.slice(1, 5).map((call) => call.quality),
    [80, 60, 40, 20],
  );
  assert.deepEqual(fixture.calls[5], {
    format: "jpeg",
    width: 1000,
    height: 500,
    quality: 20,
  });
  assert.equal(fixture.calls.length, 11);
});

test("PNG attempts a palette before JPEG and keeps alpha when the palette fits", async () => {
  const fixture = codecFixture({
    format: "png",
    width: 100,
    height: 100,
    encode: () => ({ size: 200 }),
  });
  const image = await prepareClaudeImage(fixture.bytes, fixture.loader);
  assert.equal(image.mediaType, "image/png");
  assert.deepEqual(fixture.calls, [
    {
      format: "png",
      width: 100,
      height: 100,
      compressionLevel: 9,
      palette: true,
    },
  ]);
});

test("the byte budget uses a bounded search and retains its best quality within budget", async () => {
  const fixture = codecFixture({
    width: 100,
    height: 100,
    size: IMAGE_BYTE_BUDGET + 1,
    encode: ({ quality }) => ({
      size: quality <= 30 ? 100 : IMAGE_BYTE_BUDGET + 1,
    }),
  });
  assert.equal(
    (await prepareClaudeImage(fixture.bytes, fixture.loader)).bytes.length,
    100,
  );
  assert.equal(fixture.calls.length, 5);
  assert.deepEqual(
    fixture.calls.map((call) => call.quality),
    [45, 22, 33, 27, 30],
  );
});

test("the byte budget is best effort within the hard API limit", async () => {
  const fixture = codecFixture({
    width: 100,
    height: 100,
    size: IMAGE_BYTE_BUDGET + 2,
    encode: () => ({ size: IMAGE_BYTE_BUDGET + 1 }),
  });
  assert.equal(
    (await prepareClaudeImage(fixture.bytes, fixture.loader)).bytes.length,
    IMAGE_BYTE_BUDGET + 1,
  );
  assert.equal(fixture.calls.length, 5);
});

test("accepts a final result exactly at the hard raw and base64 byte limits", async () => {
  const fixture = codecFixture({ encode: () => ({ size: MAX_IMAGE_BYTES }) });
  const image = await prepareClaudeImage(fixture.bytes, fixture.loader);
  assert.equal(image.bytes.length, MAX_IMAGE_BYTES);
  assert.equal(4 * Math.ceil(image.bytes.length / 3), MAX_IMAGE_BASE64_BYTES);
});

test("an extreme portrait retains a positive width", async () => {
  const image = await prepareClaudeImage(await fixture("png", 1, 4001));
  const actual = await sharp(image.bytes).metadata();
  assert.equal(actual.width, 1);
  assert.equal(actual.height, MAX_IMAGE_SIDE);
});

test("checks dimensions on the actual output after compression", async () => {
  const fixture = codecFixture({
    encode: () => ({ size: 100, metadata: { width: MAX_IMAGE_SIDE + 1 } }),
  });
  await assert.rejects(
    prepareClaudeImage(fixture.bytes, fixture.loader),
    /Cannot send image.*limits/,
  );
});

test("refuses a failed encode instead of returning the original", async () => {
  const fixture = codecFixture({
    encode: () => {
      throw Error("decode failed");
    },
  });
  await assert.rejects(
    prepareClaudeImage(fixture.bytes, fixture.loader),
    /Cannot send image.*could not read or compress/,
  );
});

test("refuses missing dimensions", async () => {
  await assert.rejects(
    prepareClaudeImage(
      Buffer.from("header without dimensions"),
      async () => () => ({
        metadata: async () => ({ format: "png" }),
      }),
    ),
    /Cannot send image.*dimensions cannot be verified/,
  );
});

for (const format of ["gif", "webp"]) {
  for (const width of [32, MAX_IMAGE_SIDE + 1]) {
    test(`${format}: animation at width ${width} preserves or selects the first frame`, async () => {
      const height = 10;
      const pixels = Buffer.alloc(width * height * 2 * 4);
      for (let offset = 0; offset < pixels.length; offset += 4) {
        pixels[offset] = offset < pixels.length / 2 ? 255 : 0;
        pixels[offset + 2] = offset < pixels.length / 2 ? 0 : 255;
        pixels[offset + 3] = 255;
      }
      const original = await sharp(pixels, {
        raw: { width, height: height * 2, channels: 4, pageHeight: height },
      })
        [format]({ delay: [100, 100], lossless: true })
        .toBuffer();
      assert.equal((await sharp(original).metadata()).pages, 2);
      const image = await prepareClaudeImage(original);
      if (width <= MAX_IMAGE_SIDE) assert.deepEqual(image.bytes, original);
      else {
        const actual = await sharp(image.bytes).metadata();
        assert.equal(actual.width, MAX_IMAGE_SIDE);
        assert.equal(actual.height, height);
        assert.equal(actual.pages ?? 1, 1);
        const pixel = await sharp(image.bytes)
          .extract({ left: 0, top: 0, width: 1, height: 1 })
          .raw()
          .toBuffer();
        assert.ok(pixel[0] > 240 && pixel[2] < 10, "the first frame is red");
      }
    });
  }
}

test("a real PNG over the raw byte limit reaches the byte budget", async () => {
  const width = 1536,
    height = 1024;
  // An uncompressed PNG crosses the byte limit without an expensive noise fixture.
  const bytes = await sharp({
    create: { width, height, channels: 3, background: "#2080c0" },
  })
    .png({ compressionLevel: 0 })
    .toBuffer();
  assert.ok(bytes.length > MAX_IMAGE_BYTES);
  const image = await prepareClaudeImage(bytes);
  const actual = await sharp(image.bytes).metadata();
  assert.equal(actual.width, width);
  assert.equal(actual.height, height);
  assert.ok(image.bytes.length <= IMAGE_BYTE_BUDGET);
});

test("required platform codecs match sharp's pinned native versions", async () => {
  const manifest = JSON.parse(
    await fs.readFile(new URL("./package.json", import.meta.url), "utf8"),
  );
  const native = JSON.parse(
    await fs.readFile(
      new URL("./node_modules/sharp/package.json", import.meta.url),
      "utf8",
    ),
  );
  for (const platform of ["darwin-arm64", "linux-arm64", "linux-x64"])
    for (const name of [
      `@img/sharp-${platform}`,
      `@img/sharp-libvips-${platform}`,
    ])
      assert.equal(
        manifest.dependencies[name],
        native.optionalDependencies[name],
        `${name} must survive --no-optional production deploy`,
      );
});
