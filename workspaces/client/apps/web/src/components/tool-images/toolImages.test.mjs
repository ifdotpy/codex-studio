import assert from "node:assert/strict";
import { toolImages, toolImageDisplayPayload } from "./toolImages.ts";

import { it } from "vitest";

it("classifies image paths and content without exposing raw image bytes", () => {
  const uri = "data:image/png;base64,aW1hZ2U=";
  const path = "/tmp/screenshot #1%:2";
  assert.deepEqual(toolImages({ type: "imageView", path }), [
    { kind: "path", source: path, name: "screenshot #1%:2" },
  ]);
  assert.deepEqual(toolImages({ type: "imageView", path: "/tmp/" }), [
    { kind: "path", source: "/tmp/", name: "Image" },
  ]);
  for (const invalidPath of ["", 42, null, "bad\u0000path"]) {
    assert.deepEqual(toolImages({ type: "imageView", path: invalidPath }), []);
  }
  assert.deepEqual(toolImages({ type: "other", path }), []);
  assert.deepEqual(
    toolImages({
      type: "other",
      tool: "view_image",
      arguments: { path },
    }),
    [],
  );
  assert.deepEqual(
    toolImages({
      type: "dynamicToolCall",
      tool: "view_image",
      arguments: { path },
    }),
    toolImages({ type: "imageView", path }),
  );
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
  assert.deepEqual(
    toolImages({
      type: "dynamicToolCall",
      tool: "view_image",
      arguments: null,
    }),
    [],
  );
  assert.deepEqual(
    toolImages({
      type: "dynamicToolCall",
      tool: "view_image",
      arguments: ['{"path":"/tmp/array-argument"}'],
    }),
    [],
    "Only string arguments are parsed as JSON",
  );
  assert.deepEqual(
    toolImages({ type: "imageView", path: "bad\u0000path" }),
    [],
  );
  const input = {
    type: "dynamicToolCall",
    contentItems: [
      { type: "inputText", text: "Result text" },
      { type: "inputImage", imageUrl: uri, id: "first" },
      { type: "inputImage", imageUrl: uri, id: "duplicate" },
    ],
  };
  assert.deepEqual(toolImages(input), [
    { kind: "inline", source: uri, name: "Tool image" },
  ]);
  const displayInput = toolImageDisplayPayload(input);
  assert.deepEqual(displayInput.contentItems[1], {
    type: "inputImage",
    imageUrl: "[Image content]",
    id: "first",
  });
  assert.deepEqual(displayInput.contentItems[2], {
    type: "inputImage",
    imageUrl: "[Image content]",
    id: "duplicate",
  });
  assert.equal(displayInput.contentItems[0].text, "Result text");
  assert.ok(
    !JSON.stringify(displayInput).includes("aW1hZ2U="),
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
        { type: "image", mimeType: "image/png", data: "aW1hZ2U=" },
        { type: "text", text: "MCP text" },
        null,
      ],
      isError: false,
    },
  };
  assert.deepEqual(toolImages(mcp), [
    { kind: "inline", source: uri, name: "Tool image" },
  ]);
  const displayMcp = toolImageDisplayPayload(mcp);
  assert.deepEqual(displayMcp.result.content[0], {
    type: "image",
    mimeType: "image/png",
    data: "[Image content]",
  });
  assert.equal(displayMcp.result.content[2].text, "MCP text");
  assert.equal(displayMcp.result.isError, false);
  assert.equal(displayMcp.result.content[3], null);
  assert.ok(!JSON.stringify(displayMcp).includes("aW1hZ2U="));
  for (const source of [
    "prefix data:image/png;base64,aW1hZ2U=",
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
  assert.deepEqual(
    toolImages({
      type: "other",
      contentItems: [{ type: "inputImage", imageUrl: uri }],
    }),
    [],
    "Image-like fields on unrelated events are ignored",
  );
  assert.deepEqual(
    toolImages({
      type: "dynamicToolCall",
      contentItems: "not an array",
    }),
    [],
  );
  assert.deepEqual(
    toolImages({ type: "mcpToolCall", result: { content: "not an array" } }),
    [],
  );
  assert.deepEqual(toolImages({ type: "mcpToolCall" }), []);
  assert.deepEqual(
    toolImages({
      type: "other",
      result: {
        content: [{ type: "image", mimeType: "image/png", data: "bytes" }],
      },
    }),
    [],
    "MCP image result fields on other events are ignored",
  );
  assert.deepEqual(
    toolImages({
      type: "mcpToolCall",
      result: {
        content: [
          { type: "image", mimeType: "image/svg+xml", data: "svg-bytes" },
          { type: "image", mimeType: "text/plain", data: "plain-text" },
          { type: "image", mimeType: "not-image/png", data: "bad-prefix" },
          { type: "image", mimeType: "image/png-extra", data: "bad-suffix" },
          {
            type: "image",
            mimeType: "image/png;base64,AAAA",
            data: "BBBB",
          },
          { type: "image", mimeType: "image/png", data: 23 },
          { type: "text", mimeType: "image/png", data: "not-an-image-block" },
        ],
      },
    }),
    [],
  );
  assert.deepEqual(
    toolImages({
      type: "mcpToolCall",
      result: {
        content: [
          { type: "image", mimeType: "image/PNG", data: "bytes" },
          { type: "image", mimeType: "image/jpeg", data: "jpeg" },
        ],
      },
    }),
    [
      {
        kind: "inline",
        source: "data:image/PNG;base64,bytes",
        name: "Tool image",
      },
      {
        kind: "inline",
        source: "data:image/jpeg;base64,jpeg",
        name: "Tool image",
      },
    ],
  );
  assert.deepEqual(
    toolImages({
      type: "dynamicToolCall",
      contentItems: [
        null,
        { type: "inputImage", imageUrl: [uri] },
        { type: "inputImage", imageUrl: uri },
      ],
      result: { content: null },
    }),
    [{ kind: "inline", source: uri, name: "Tool image" }],
  );
  assert.deepEqual(
    toolImages({
      type: "dynamicToolCall",
      tool: "view_image",
      arguments: { path: uri },
      contentItems: [{ type: "inputImage", imageUrl: uri }],
    }),
    [
      { kind: "path", source: uri, name: "png;base64,aW1hZ2U=" },
      { kind: "inline", source: uri, name: "Tool image" },
    ],
    "A path and inline image with the same source remain distinct kinds",
  );
  assert.deepEqual(
    toolImageDisplayPayload({
      type: "dynamicToolCall",
      contentItems: [null, { type: "inputText", text: "kept" }],
      result: null,
    }),
    {
      type: "dynamicToolCall",
      contentItems: [null, { type: "inputText", text: "kept" }],
      result: null,
    },
  );
  console.log(
    "PASS: native imageView path, explicit dynamic path, native/MCP inline images, bounded display, and unsupported payloads",
  );
});
