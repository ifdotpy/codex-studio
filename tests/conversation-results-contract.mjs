import assert from "node:assert/strict";
import { registerHooks } from "node:module";
const hooks = registerHooks({
  resolve(specifier, context, next) {
    if (
      specifier === "./fileLinks" &&
      context.parentURL?.endsWith("conversationResultModel.ts")
    )
      specifier += ".ts";
    return next(specifier, context);
  },
});
const { conversationResults } = await import(
  "../web/src/components/conversationResultModel.ts"
);
hooks.deregister();
const assistant = (text, extra = {}) => ({
  id: "answer",
  role: "assistant",
  text,
  ...extra,
});
const output = (payload, extra = {}) => ({
  id: "tool",
  role: "output",
  text: JSON.stringify(payload),
  ...extra,
});
let count = 0;
function check(name, run) {
  run();
  count++;
  console.log(`PASS ${name}`);
}

check("linked paths preserve labels, line numbers and local images", () => {
  const results = conversationResults([
    assistant(
      "[Build report](</work/my report.md:12>) ![Preview](/work/view.png) [Again](/work/my%20report.md#L30)",
    ),
  ]);
  assert.equal(results.length, 2);
  assert.deepEqual(
    { label: results[0].label, path: results[0].path, line: results[0].line },
    { label: "Build report", path: "/work/my report.md", line: 12 },
  );
  assert.equal(results[1].image, true);
});
check(
  "links in nested lists and reference links use the Markdown lexer",
  () => {
    const results = conversationResults([
      assistant(
        "- **See [report][r]**\n\n[r]: docs/report.md\n\n`[fake](secret.txt)`",
      ),
    ]);
    assert.equal(results.length, 1);
    assert.equal(results[0].path, "docs/report.md");
  },
);
check(
  "external URLs, broken escapes, remote files, and unlinked paths do not become results",
  () => {
    const results = conversationResults([
      assistant(
        "[Web](https://example.com/a) ![Tracker](https://example.com/pixel.png) [Bad](file://remote/secret) [Broken](/bad%ZZ) [Jump](#heading) Plain /work/report.md\n\n```sh\ncat /work/private.md\n```",
      ),
      output({
        type: "commandExecution",
        commandActions: [{ type: "read", path: "/work/private.md" }],
      }),
    ]);
    assert.deepEqual(results, []);
  },
);
check(
  "only agent attachments are outputs and duplicate attachment IDs collapse",
  () => {
    const assets = [{ id: "asset-1", name: "diagram.svg", image: true }];
    const results = conversationResults([
      assistant("", { assets: [...assets, ...assets] }),
      { id: "user", role: "user", text: "Input", assets },
    ]);
    assert.equal(results.length, 1);
    assert.equal(results[0].kind, "asset");
  },
);
check(
  "recorded completed patch content is preserved without reading the workspace",
  () => {
    const diff = "@@ -1 +1 @@\n-before\n+historical\n";
    const results = conversationResults([
      output({
        type: "fileChange",
        status: "completed",
        changes: [{ path: "src/old.ts", diff }],
      }),
    ]);
    assert.equal(results[0].source, diff);
    assert.equal(results[0].label, "old.ts");
    assert.deepEqual(results[0].paths, ["src/old.ts"]);
    assert.equal(results[0].kind, "patch");
  },
);
check(
  "failed, declined, running, user, and arbitrary tool payloads do not report produced changes",
  () => {
    const change = {
      type: "fileChange",
      changes: [{ path: "secret.ts", diff: "+not applied" }],
    };
    const results = conversationResults([
      output({ ...change, status: "failed" }),
      output({ ...change, status: "declined" }),
      output({ ...change, status: "inProgress" }),
      output(change, { toolStatus: "running" }),
      output(change, { toolStatus: "failed" }),
      output(change),
      output({ ...change, status: "completed", success: false }),
      output({ ...change, status: "completed", error: "Rejected" }),
      output({ type: "read_file", path: "secret.ts", diff: "+read" }),
      assistant("[Pending](a.md)", { streaming: true }),
    ]);
    assert.deepEqual(results, []);
  },
);
check(
  "turn diff provides recorded paths including deleted files and flags clipped evidence",
  () => {
    const diff =
      "--- a/deleted.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n--- /dev/null\n+++ b/new file.txt\n@@ -0,0 +1 @@\n+new";
    const results = conversationResults([
      output({ turnId: "turn-7", diff }, { title: "Changes", truncated: true }),
    ]);
    assert.deepEqual(results[0].paths, ["deleted.txt", "new file.txt"]);
    assert.equal(results[0].source, diff);
    assert.equal(results[0].truncated, true);
  },
);
check(
  "rich output retains exact source, merges raw HTML, and excludes unfinished fences",
  () => {
    const results = conversationResults([
      assistant(
        "```mermaid\ngraph TD; A-->B\n```\n\n<style>p{color:red}</style>\n\n<div>Preview</div>\n\n```html\n<p>Unfinished",
      ),
    ]);
    assert.equal(results.length, 2);
    assert.equal(results[0].kind, "mermaid");
    assert.equal(results[0].source, "graph TD; A-->B");
    assert.equal(results[1].kind, "html");
    assert.match(results[1].source, /<style>.*<\/style>[\s\S]*<div>/);
  },
);
check(
  "repeated loaded output collapses, distinct revisions remain available",
  () => {
    const results = conversationResults([
      assistant("[Report](./report.md)\n\n```html\n<p>One</p>\n```"),
      assistant("[Report](report.md)\n\n```html\n<p>One</p>\n```", {
        id: "answer-2",
      }),
      assistant("```html\n<p>Two</p>\n```", { id: "answer-3" }),
    ]);
    assert.equal(results.length, 3);
    assert.equal(results[2].messageId, "answer-3");
  },
);
console.log(`${count} conversation result contracts passed`);
