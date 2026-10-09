import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";

const run = promisify(execFile);
// Anthropic recommends at most 1568 px on the long side. Requests with many
// images reject any image above 2000 px and remove it from the conversation.
export const MAX_IMAGE_SIDE = 1568;
export const MAX_IMAGE_BYTES = 3_750_000;

async function dimensions(file) {
  const { stdout } = await run(
    "sips",
    ["-g", "pixelWidth", "-g", "pixelHeight", file],
    {
      timeout: 15000,
    },
  );
  const width = Number(/pixelWidth:\s*(\d+)/.exec(stdout)?.[1]);
  const height = Number(/pixelHeight:\s*(\d+)/.exec(stdout)?.[1]);
  return Number.isFinite(width) && Number.isFinite(height)
    ? { width, height }
    : null;
}

/** Return image bytes that the Claude API accepts in long conversations.
 * The source file is never changed. Without sips the original bytes are kept. */
export async function claudeImage(file, mediaType) {
  const bytes = await fs.readFile(file);
  if (mediaType === "image/gif") return { bytes, mediaType };
  let size;
  try {
    size = await dimensions(file);
  } catch {
    return { bytes, mediaType };
  }
  if (!size) return { bytes, mediaType };
  const large = Math.max(size.width, size.height) > MAX_IMAGE_SIDE;
  if (!large && bytes.length <= MAX_IMAGE_BYTES) return { bytes, mediaType };
  const directory = await fs.mkdtemp(
    path.join(os.tmpdir(), "studio-claude-image-"),
  );
  try {
    const png = path.join(directory, randomUUID() + ".png");
    const args = large ? ["-Z", String(MAX_IMAGE_SIDE)] : [];
    await run("sips", [...args, "-s", "format", "png", file, "--out", png], {
      timeout: 30000,
    });
    let result = { bytes: await fs.readFile(png), mediaType: "image/png" };
    if (result.bytes.length > MAX_IMAGE_BYTES) {
      const jpeg = path.join(directory, randomUUID() + ".jpg");
      await run(
        "sips",
        [
          "-s",
          "format",
          "jpeg",
          "-s",
          "formatOptions",
          "85",
          png,
          "--out",
          jpeg,
        ],
        {
          timeout: 30000,
        },
      );
      result = { bytes: await fs.readFile(jpeg), mediaType: "image/jpeg" };
    }
    return result;
  } catch {
    return { bytes, mediaType };
  } finally {
    await fs.rm(directory, { recursive: true, force: true });
  }
}
