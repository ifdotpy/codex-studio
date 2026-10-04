import type { Json, JsonValue } from "../../types";

export interface ToolImage {
  kind: "path" | "inline";
  source: string;
  name: string;
}
const inlineImage = /^data:image\/(png|jpeg|gif|webp);base64,/i;

function objectValue(value: unknown): Record<string, JsonValue> | null {
  return isJsonObject(value) ? value : null;
}

function isJsonObject(value: unknown): value is Record<string, JsonValue> {
  return (
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    Object.values(value).every(isJsonValue)
  );
}

function isJsonValue(value: unknown): value is JsonValue {
  if (
    value === null ||
    typeof value === "string" ||
    typeof value === "boolean" ||
    (typeof value === "number" && Number.isFinite(value))
  )
    return true;
  if (Array.isArray(value)) return value.every(isJsonValue);
  return isJsonObject(value);
}

// Native ThreadItem.imageView supplies path. Dynamic tools use inputImage;
// MCP results use image content blocks. Do not search arbitrary JSON fields.
export function toolImages(payload: Json): ToolImage[] {
  const images: ToolImage[] = [];
  const path = (value: unknown) => {
    // oxlint-disable-next-line no-control-regex -- Reject control characters in paths.
    if (typeof value !== "string" || !value || /[\u0000-\u001f]/.test(value))
      return;
    images.push({
      kind: "path",
      source: value,
      name: value.split(/[\\/]/).at(-1) || "Image",
    });
  };
  if (payload.type === "imageView") path(payload.path);
  if (payload.type === "dynamicToolCall" && payload.tool === "view_image") {
    let args: unknown = payload.arguments;
    if (typeof args === "string") {
      try {
        args = JSON.parse(args);
      } catch {}
    }
    path(objectValue(args)?.path);
  }
  const inline = (source: unknown) => {
    if (typeof source === "string" && inlineImage.test(source))
      images.push({ kind: "inline", source, name: "Tool image" });
  };
  if (payload.type === "dynamicToolCall" && Array.isArray(payload.contentItems))
    for (const value of payload.contentItems) {
      const block = objectValue(value);
      if (block?.type === "inputImage") inline(block.imageUrl);
    }
  const result = objectValue(payload.result);
  const content = result?.content;
  if (payload.type === "mcpToolCall" && Array.isArray(content))
    for (const value of content) {
      const block = objectValue(value);
      if (
        block?.type === "image" &&
        typeof block.data === "string" &&
        typeof block.mimeType === "string" &&
        /^image\/(png|jpeg|gif|webp)$/i.test(block.mimeType)
      )
        images.push({
          kind: "inline",
          source: `data:${block.mimeType};base64,${block.data}`,
          name: "Tool image",
        });
    }
  return images.filter(
    (image, index) =>
      images.findIndex(
        (other) => other.kind === image.kind && other.source === image.source,
      ) === index,
  );
}

// Preserve text and errors in the raw event without serializing image data again.
export function toolImageDisplayPayload(payload: Json): Json {
  const block = (value: JsonValue): JsonValue => {
    const object = objectValue(value);
    if (!object) return value;
    if (object.type === "inputImage")
      return { ...object, imageUrl: "[Image content]" };
    if (object.type === "image") return { ...object, data: "[Image content]" };
    return value;
  };
  const result = objectValue(payload.result);
  const contentItems = payload.contentItems;
  const resultContent = result?.content;
  return {
    ...payload,
    ...(Array.isArray(contentItems)
      ? { contentItems: contentItems.map(block) }
      : {}),
    ...(result && Array.isArray(resultContent)
      ? {
          result: { ...result, content: resultContent.map(block) },
        }
      : {}),
  };
}
