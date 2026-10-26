import type { SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import type { Metadata, ResizeOptions, PngOptions, JpegOptions } from "sharp";
import fs from "node:fs/promises";
import path from "node:path";

// Claude Code 2.1.302: chunk-sp5b4hyj.js:453,475; chunk-hqtnbrjd.js:124661.
// These limits own the bridge's default image dimensions (pixels) and byte budgets.
export const MAX_IMAGE_SIDE = 2000;
export const MAX_IMAGE_BASE64_BYTES = 5 * 1024 * 1024;
export const MAX_IMAGE_BYTES = (MAX_IMAGE_BASE64_BYTES / 4) * 3;
export const IMAGE_BYTE_BUDGET = 512_000;
const JPEG_QUALITIES = [80, 60, 40, 20];
const BYTE_BUDGET_PASSES = 5;
const FINAL_IMAGE_WIDTH = 1000;
type ImageBlock = Extract<
  Exclude<SDKUserMessage["message"]["content"], string>[number],
  { type: "image" }
>;
type ImageMediaType = Extract<
  ImageBlock["source"],
  { type: "base64" }
>["media_type"];
const MEDIA_TYPES = new Map<string, ImageMediaType>([
  ["jpeg", "image/jpeg"],
  ["png", "image/png"],
  ["gif", "image/gif"],
  ["webp", "image/webp"],
]);

export type ImageMetadata = Partial<
  Pick<Metadata, "format" | "width" | "height">
>;
export type ImagePipeline = {
  metadata(): Promise<ImageMetadata>;
  resize(width: number, height: number, options: ResizeOptions): ImagePipeline;
  png(options?: PngOptions): ImagePipeline;
  jpeg(options?: JpegOptions): ImagePipeline;
  toBuffer(): Promise<Buffer>;
};
export type ImageCodec = (bytes: Buffer) => ImagePipeline;

class ImagePreparationError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super("Cannot send image: " + message, options);
    this.name = "ImagePreparationError";
  }
}

async function loadSharp() {
  // Keep the bridge usable for text if a native codec is absent or cannot load.
  try {
    return (await import("sharp")).default;
  } catch (cause) {
    throw new ImagePreparationError(
      "the image processor is unavailable. Reinstall Studio and try again.",
      { cause },
    );
  }
}

function dimensions(metadata: ImageMetadata) {
  const { width, height } = metadata;
  if (
    typeof width !== "number" ||
    !Number.isSafeInteger(width) ||
    typeof height !== "number" ||
    !Number.isSafeInteger(height) ||
    width < 1 ||
    height < 1
  )
    throw new ImagePreparationError(
      "the image dimensions cannot be verified. Use a valid PNG, JPEG, GIF, or WebP image.",
    );
  return { width, height };
}

function targetDimensions({ width, height }: ReturnType<typeof dimensions>) {
  if (width > MAX_IMAGE_SIDE) {
    height = Math.max(1, Math.round((height * MAX_IMAGE_SIDE) / width));
    width = MAX_IMAGE_SIDE;
  }
  if (height > MAX_IMAGE_SIDE) {
    width = Math.max(1, Math.round((width * MAX_IMAGE_SIDE) / height));
    height = MAX_IMAGE_SIDE;
  }
  return { width, height };
}

async function reduceImage(
  sharp: ImageCodec,
  bytes: Buffer,
  metadata: ImageMetadata,
) {
  const original = dimensions(metadata);
  const target = targetDimensions(original);
  const resize =
    target.width !== original.width || target.height !== original.height;
  if (!resize && bytes.length <= MAX_IMAGE_BYTES) return bytes;
  // Sharp's default input selects the first frame, as the native Claude pipeline does.
  const image = () =>
    sharp(bytes).resize(target.width, target.height, {
      fit: "inside",
      withoutEnlargement: true,
    });
  const compress = async () => {
    if (metadata.format === "png") {
      const palette = await image()
        .png({ compressionLevel: 9, palette: true })
        .toBuffer();
      if (palette.length <= MAX_IMAGE_BYTES) return palette;
    }
    for (const quality of JPEG_QUALITIES) {
      const jpeg = await image().jpeg({ quality }).toBuffer();
      if (jpeg.length <= MAX_IMAGE_BYTES) return jpeg;
    }
  };
  // Try compression before a format conversion when only the byte limit is exceeded.
  if (!resize) {
    const compressed = await compress();
    if (compressed) return compressed;
  }
  const resized = metadata.format === "gif" ? image().png() : image();
  const result = await resized.toBuffer();
  if (result.length <= MAX_IMAGE_BYTES) return result;
  if (resize) {
    const compressed = await compress();
    if (compressed) return compressed;
  }
  const width = Math.min(target.width, FINAL_IMAGE_WIDTH);
  const height = Math.max(
    1,
    Math.round((target.height * width) / target.width),
  );
  return sharp(bytes)
    .resize(width, height, { fit: "inside", withoutEnlargement: true })
    .jpeg({ quality: 20 })
    .toBuffer();
}

// Claude's secondary byte budget is best effort. Retain the smallest candidate if
// no quality meets it. The API's hard byte and dimension limits are checked below.
async function budgetImage(sharp: ImageCodec, bytes: Buffer) {
  if (bytes.length <= IMAGE_BYTE_BUDGET) return bytes;
  let smallest = bytes;
  let upper = 90;
  const encode = (quality: number) => sharp(bytes).jpeg({ quality }).toBuffer();
  if ((await sharp(bytes).metadata()).format !== "jpeg") {
    const jpeg = await encode(90);
    if (jpeg.length < smallest.length) smallest = jpeg;
    if (jpeg.length <= IMAGE_BYTE_BUDGET) return jpeg;
    upper = 89;
  }
  let lower = 1;
  let withinBudget;
  for (let attempt = 0; attempt < BYTE_BUDGET_PASSES; attempt++) {
    const quality = Math.floor((lower + upper) / 2);
    const jpeg = await encode(quality);
    if (jpeg.length < smallest.length) smallest = jpeg;
    if (jpeg.length <= IMAGE_BYTE_BUDGET) {
      withinBudget = jpeg;
      lower = quality + 1;
    } else upper = quality - 1;
    if (lower > upper) break;
  }
  return withinBudget ?? smallest;
}

/** Prepare bytes with portable codecs. A codec loader permits deterministic failure tests. */
export async function prepareClaudeImage(
  bytes: Buffer,
  codecLoader: () => Promise<ImageCodec> = loadSharp,
) {
  try {
    if (!bytes.length)
      throw new ImagePreparationError("the image file is empty.");
    const sharp = await codecLoader();
    const metadata = await sharp(bytes).metadata();
    if (!MEDIA_TYPES.has(metadata.format ?? ""))
      throw new ImagePreparationError(
        "the image format is unsupported. Use PNG, JPEG, GIF, or WebP.",
      );
    const result = await budgetImage(
      sharp,
      await reduceImage(sharp, bytes, metadata),
    );
    const final = await sharp(result).metadata();
    const { width, height } = dimensions(final);
    const mediaType = MEDIA_TYPES.get(final.format ?? "");
    // Validate the actual final bytes, including base64 padding, after every encode.
    if (
      !mediaType ||
      width > MAX_IMAGE_SIDE ||
      height > MAX_IMAGE_SIDE ||
      result.length > MAX_IMAGE_BYTES ||
      4 * Math.ceil(result.length / 3) > MAX_IMAGE_BASE64_BYTES
    )
      throw new ImagePreparationError(
        `compression could not meet the ${MAX_IMAGE_SIDE}x${MAX_IMAGE_SIDE}px and ${MAX_IMAGE_BASE64_BYTES}-byte base64 limits. Use a smaller image.`,
      );
    return { bytes: result, mediaType };
  } catch (cause) {
    if (cause instanceof ImagePreparationError) throw cause;
    throw new ImagePreparationError(
      "the image processor could not read or compress this image. Use a smaller PNG, JPEG, GIF, or WebP image.",
      { cause },
    );
  }
}

/** The source file is never changed. Unchecked bytes never reach the SDK. */
export async function claudeImage(file: string) {
  try {
    return await prepareClaudeImage(await fs.readFile(file));
  } catch (cause) {
    const message =
      cause instanceof ImagePreparationError
        ? cause.message
        : "Cannot send image: the image file cannot be read.";
    throw new Error(`${path.basename(file)}: ${message}`, { cause });
  }
}
