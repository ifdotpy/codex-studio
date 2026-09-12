import type { Json } from "../types";

export interface ToolImage {
  kind: "path" | "inline";
  source: string;
  name: string;
}
const inlineImage = /^data:image\/(png|jpeg|gif|webp);base64,/i;

// Native ThreadItem.imageView supplies path. Dynamic tools use inputImage;
// MCP results use image content blocks. Do not search arbitrary JSON fields.
export function toolImages(payload: Json): ToolImage[] {
  const images: ToolImage[] = [];
  const path = (value: unknown) => {
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
    let args = payload.arguments;
    if (typeof args === "string") {
      try {
        args = JSON.parse(args);
      } catch {
        args = null;
      }
    }
    path(args?.path);
  }
  const inline = (source: unknown) => {
    if (typeof source === "string" && inlineImage.test(source))
      images.push({ kind: "inline", source, name: "Tool image" });
  };
  if (payload.type === "dynamicToolCall")
    for (const block of Array.isArray(payload.contentItems)
      ? payload.contentItems
      : [])
      if (block?.type === "inputImage") inline(block.imageUrl);
  if (payload.type === "mcpToolCall")
    for (const block of Array.isArray(payload.result?.content)
      ? payload.result.content
      : [])
      if (
        block?.type === "image" &&
        typeof block.data === "string" &&
        /^image\/(png|jpeg|gif|webp)$/i.test(block.mimeType || "")
      )
        inline(`data:${block.mimeType};base64,${block.data}`);
  return images.filter(
    (image, index) =>
      images.findIndex(
        (other) => other.kind === image.kind && other.source === image.source,
      ) === index,
  );
}

// Preserve text and errors in the raw event without serializing image data again.
export function toolImageDisplayPayload(payload: Json): Json {
  const block = (value: Json) => {
    if (value?.type === "inputImage")
      return { ...value, imageUrl: "[Image content]" };
    if (value?.type === "image") return { ...value, data: "[Image content]" };
    return value;
  };
  return {
    ...payload,
    ...(Array.isArray(payload.contentItems)
      ? { contentItems: payload.contentItems.map(block) }
      : {}),
    ...(Array.isArray(payload.result?.content)
      ? {
          result: {
            ...payload.result,
            content: payload.result.content.map(block),
          },
        }
      : {}),
  };
}
