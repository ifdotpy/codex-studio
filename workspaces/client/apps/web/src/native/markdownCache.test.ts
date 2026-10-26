import assert from "node:assert/strict";
import { expect, it } from "vitest";
import { MarkdownCache } from "./markdownCache";
it("preserves JSON types and property names that resemble compact tags", () => {
  const tree = JSON.parse(
    '{"0":[0,1,2,"0",null,true,false],"text":[1],"__proto__":{"index":2},"empty":{},"list":[]}',
  );
  const cache = new MarkdownCache();
  cache.set("types", tree);
  expect(cache.get("types")).toEqual(tree);
});
it("keeps exact source, returns independent trees and bounds LRU storage", () => {
  const cache = new MarkdownCache(1000, 2);
  cache.set("a", [{ text: "A" }]);
  cache.set("b", [{ text: "B" }]);
  const value = cache.get("a");
  assert.ok(Array.isArray(value));
  const item: unknown = value[0];
  assert.ok(item && typeof item === "object" && "text" in item);
  item.text = "changed";
  cache.set("c", [{ text: "C" }]);
  expect(cache.get("a")).toEqual([{ text: "A" }]);
  expect(cache.get("b")).toBeUndefined();
  expect(cache.get("a ")).toBeUndefined();
  cache.set("huge", ["x".repeat(1000)]);
  expect(cache.get("huge")).toBeUndefined();
  expect(cache.get("c")).toEqual([{ text: "C" }]);
});

it("retains repeated sentence text within the same byte budget and restores the exact tree", () => {
  const text = "Unicode 🦀 sentence. ".repeat(20);
  const tree = [
    { nodes: [{ text, sentences: [{ text, index: 0 }] }], length: text.length },
  ];
  const cache = new MarkdownCache(1600);
  cache.set("source", tree);
  expect(cache.get("source")).toEqual(tree);
  const restored = cache.get("source");
  assert.ok(Array.isArray(restored));
  const item: unknown = restored[0];
  assert.ok(item && typeof item === "object" && "nodes" in item);
  assert.ok(Array.isArray(item.nodes));
  const node: unknown = item.nodes[0];
  assert.ok(node && typeof node === "object" && "sentences" in node);
  assert.ok(Array.isArray(node.sentences));
  const sentence: unknown = node.sentences[0];
  assert.ok(sentence && typeof sentence === "object" && "text" in sentence);
  sentence.text = "changed";
  expect(cache.get("source")).toEqual(tree);
});
