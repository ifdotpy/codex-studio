import assert from "node:assert/strict";
import {
  toolImages,
  toolImageDisplayPayload,
} from "../web/src/components/toolImages.ts";
const uri = "data:image/png;base64,aW1hZ2U=";
const path = "/tmp/screenshot #1%:2";
assert.deepEqual(toolImages({ type: "imageView", path }), [
  { kind: "path", source: path, name: "screenshot #1%:2" },
]);
assert.deepEqual(
  toolImages({
    type: "dynamicToolCall",
    tool: "view_image",
    arguments: JSON.stringify({ path }),
  }),
  toolImages({ type: "imageView", path }),
);
assert.deepEqual(
  toolImages({
    type: "dynamicToolCall",
    tool: "read_file",
    arguments: { path },
  }),
  [],
  "Unrelated path arguments do not become images",
);
assert.deepEqual(
  toolImages({
    type: "dynamicToolCall",
    tool: "view_image",
    arguments: "invalid",
  }),
  [],
);
assert.deepEqual(toolImages({ type: "imageView", path: "bad\u0000path" }), []);
const input = {
  type: "dynamicToolCall",
  contentItems: [
    { type: "inputText", text: "Result text" },
    { type: "inputImage", imageUrl: uri },
  ],
};
assert.equal(toolImages(input)[0].source, uri);
assert.equal(
  toolImageDisplayPayload(input).contentItems[0].text,
  "Result text",
);
assert.ok(
  !JSON.stringify(toolImageDisplayPayload(input)).includes("aW1hZ2U="),
  "Raw event does not duplicate image bytes",
);
assert.equal(
  input.contentItems[1].imageUrl,
  uri,
  "Display formatting cannot mutate stored data",
);
const mcp = {
  type: "mcpToolCall",
  result: {
    content: [
      { type: "image", mimeType: "image/png", data: "aW1hZ2U=" },
      { type: "text", text: "MCP text" },
    ],
  },
};
assert.equal(toolImages(mcp)[0].source, uri);
assert.equal(toolImageDisplayPayload(mcp).result.content[1].text, "MCP text");
assert.ok(!JSON.stringify(toolImageDisplayPayload(mcp)).includes("aW1hZ2U="));
for (const source of [
  "https://external.invalid/image.png",
  "javascript:alert(1)",
  "data:text/html;base64,aW1hZ2U=",
  "data:image/svg+xml;base64,aW1hZ2U=",
]) {
  assert.deepEqual(
    toolImages({
      type: "dynamicToolCall",
      contentItems: [{ type: "inputImage", imageUrl: source }],
    }),
    [],
    "Only supported inline raster content is admitted automatically",
  );
}
console.log(
  "PASS: native imageView path, explicit dynamic path, native/MCP inline images, bounded display, and unsupported payloads",
);
