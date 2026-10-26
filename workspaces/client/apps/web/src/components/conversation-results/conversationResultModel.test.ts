import { required, componentAttachment } from "../componentTestFixtures";
import type { Message, JsonValue } from "../../types";
import assert from "node:assert/strict";
import { conversationResults } from "./conversationResultModel.ts";

import { it } from "vitest";

const assistant = (text: string, extra: Partial<Message> = {}): Message => ({
  id: "answer",
  role: "assistant",
  text,
  ...extra,
});
const output = (payload: JsonValue, extra: Partial<Message> = {}): Message => ({
  id: "tool",
  role: "output",
  text: JSON.stringify(payload),
  ...extra,
});
const check = it;

check("linked paths preserve labels, line numbers and local images", () => {
  const results = conversationResults([
    assistant(
      "[Build report](</work/my report.md:12>) ![Preview](/work/view.png) [Again](/work/my%20report.md#L30)",
    ),
  ]);
  const result0 = required(results[0]);
  assert.ok("path" in result0);
  assert.ok(result0.kind === "file");
  const result1 = required(results[1]);
  assert.ok("image" in result1);
  assert.equal(results.length, 2);
  assert.deepEqual(
    { label: result0.label, path: result0.path, line: result0.line },
    { label: "Build report", path: "/work/my report.md", line: 12 },
  );
  assert.equal(result1.image, true);
});
check(
  "links in nested lists and reference links use the Markdown lexer",
  () => {
    const results = conversationResults([
      assistant(
        "- **See [report][r]**\n\n[r]: docs/report.md\n\n`[fake](secret.txt)`",
      ),
    ]);
    const result0 = required(results[0]);
    assert.ok("path" in result0);
    assert.equal(results.length, 1);
    assert.equal(result0.path, "docs/report.md");
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
    const assets = [
      componentAttachment({ id: "asset-1", name: "diagram.svg", image: true }),
    ];
    const results = conversationResults([
      assistant("", { assets: [...assets, ...assets] }),
      { id: "user", role: "user", text: "Input", assets },
      { id: "user-link", role: "user", text: "[Input](private.md)" },
    ]);
    const result0 = required(results[0]);
    assert.equal(results.length, 1);
    assert.equal(result0.kind, "asset");
  },
);
check("missing assistant text still exposes recorded attachments", () => {
  const results = conversationResults([
    // @ts-expect-error A legacy attachment-only assistant row has no required text field.
    {
      id: "answer-without-text",
      role: "assistant",
      assets: [
        componentAttachment({
          id: "saved-asset",
          name: "Saved attachment",
          image: true,
        }),
      ],
    },
  ]);
  assert.deepEqual(results, [
    {
      id: "answer-without-text:result:0",
      messageId: "answer-without-text",
      label: "Saved attachment",
      kind: "asset",
      asset: "saved-asset",
      image: true,
    },
  ]);
});
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
    const result0 = required(results[0]);
    assert.ok("source" in result0);
    assert.ok("paths" in result0);
    assert.equal(result0.source, diff);
    assert.equal(result0.label, "old.ts");
    assert.deepEqual(result0.paths, ["src/old.ts"]);
    assert.equal(result0.kind, "patch");
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
    const result0 = required(results[0]);
    assert.ok("paths" in result0);
    assert.ok("source" in result0);
    assert.ok("truncated" in result0);
    assert.deepEqual(result0.paths, ["deleted.txt", "new file.txt"]);
    assert.equal(result0.source, diff);
    assert.equal(result0.truncated, true);
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
    const result0 = required(results[0]);
    assert.ok("source" in result0);
    const result1 = required(results[1]);
    assert.ok("source" in result1);
    assert.equal(results.length, 2);
    assert.equal(result0.kind, "mermaid");
    assert.equal(result0.source, "graph TD; A-->B");
    assert.equal(result1.kind, "html");
    assert.match(result1.source, /<style>.*<\/style>[\s\S]*<div>/);
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
    const result2 = required(results[2]);
    assert.equal(results.length, 3);
    assert.equal(result2.messageId, "answer-3");
  },
);
check("turn diffs collapse against the same recorded file-change patch", () => {
  const diff = "--- a/src/shared.ts\n+++ b/src/shared.ts\n@@ -0,0 +1 @@\n+new";
  const results = conversationResults([
    output({
      type: "fileChange",
      status: "completed",
      changes: [{ path: "src/shared.ts", diff }],
    }),
    output({ diff }, { id: "turn/diff/updated", title: "Changes" }),
  ]);
  const result0 = required(results[0]);
  assert.ok("source" in result0);
  assert.ok(result0.kind === "patch");
  assert.equal(results.length, 1);
  assert.equal(result0.kind, "patch");
  assert.equal(result0.source, diff);
  assert.equal(result0.messageId, "tool");
  assert.ok(
    result0.change &&
      typeof result0.change === "object" &&
      "path" in result0.change,
  );
  assert.equal(result0.change.path, "src/shared.ts");
});
check(
  "multi-file turn diffs retain distinct identity from one file change",
  () => {
    const diff = "@@ -1 +1 @@\n-before\n+after";
    const turnDiff =
      "--- a/first.ts\n+++ b/first.ts\n--- a/second.ts\n+++ b/second.ts\n" +
      diff;
    const results = conversationResults([
      output({
        type: "fileChange",
        status: "completed",
        changes: [{ path: "Stryker was here!", diff }],
      }),
      output(
        {
          type: "fileChange",
          status: "completed",
          changes: [{ path: "first.ts", diff: turnDiff }],
        },
        { id: "file-change:multi-path" },
      ),
      output({ diff: turnDiff }, { id: "turn/diff/updated", title: "Changes" }),
      output(
        { diff },
        { id: "turn/diff/updated:without-paths", title: "Changes" },
      ),
    ]);
    const result0 = required(results[0]);
    const result2 = required(results[2]);
    assert.ok("paths" in result2);
    const result3 = required(results[3]);
    assert.ok("paths" in result3);
    assert.equal(results.length, 4);
    assert.equal(result0.kind, "patch");
    assert.deepEqual(result2.paths, ["first.ts", "second.ts"]);
    assert.deepEqual(result3.paths, []);
  },
);

check(
  "file result labels, image types, identities, and IDs match the preview list",
  () => {
    const results = conversationResults([
      assistant(
        "[  **<b>Styled</b>**  ](docs/report.md) [](/work/fallback.JpEg) ![Photo](/work/photo.JPG) ![Extensionless](/work/preview) [Jpeg](/work/photo.jpg) [Query](/work/icon.svg?download=1) [Again](./docs/report.md) [Vector](/work/icon.svg) [Nested](x/./report.md) [Plain](x/report.md)",
      ),
    ]);
    assert.deepEqual(
      results.map((result) => {
        assert.ok(result.kind === "file");
        const { id, label, kind, path, line, image } = result;
        return {
          id,
          label,
          kind,
          path,
          line,
          image,
        };
      }),
      [
        {
          id: "answer:result:0",
          label: "Styled",
          kind: "file",
          path: "docs/report.md",
          line: undefined,
          image: false,
        },
        {
          id: "answer:result:1",
          label: "fallback.JpEg",
          kind: "file",
          path: "/work/fallback.JpEg",
          line: undefined,
          image: true,
        },
        {
          id: "answer:result:2",
          label: "Photo",
          kind: "file",
          path: "/work/photo.JPG",
          line: undefined,
          image: true,
        },
        {
          id: "answer:result:3",
          label: "Extensionless",
          kind: "file",
          path: "/work/preview",
          line: undefined,
          image: true,
        },
        {
          id: "answer:result:4",
          label: "Jpeg",
          kind: "file",
          path: "/work/photo.jpg",
          line: undefined,
          image: true,
        },
        {
          id: "answer:result:5",
          label: "Query",
          kind: "file",
          path: "/work/icon.svg?download=1",
          line: undefined,
          image: false,
        },
        {
          id: "answer:result:7",
          label: "Vector",
          kind: "file",
          path: "/work/icon.svg",
          line: undefined,
          image: true,
        },
        {
          id: "answer:result:8",
          label: "Nested",
          kind: "file",
          path: "x/./report.md",
          line: undefined,
          image: false,
        },
        {
          id: "answer:result:9",
          label: "Plain",
          kind: "file",
          path: "x/report.md",
          line: undefined,
          image: false,
        },
      ],
    );
  },
);

check("only closed supported fences become previews", () => {
  const results = conversationResults([
    assistant("```HTML\nValid markup\n```\n"),
    assistant("~~~mermaid\ngraph LR; A-->B\n~~~", { id: "answer-2" }),
    assistant("~~~html\nTilde markup\n~~~", { id: "answer-18" }),
    assistant("````html\nLong fence\n```\n````", { id: "answer-3" }),
    assistant("```html\nWrong closer\n~~~", { id: "answer-4" }),
    assistant("```html\nToo short\n``", { id: "answer-5" }),
    assistant("```js\nnot a preview\n```", { id: "answer-6" }),
    assistant("```\nuntagged code\n```", { id: "answer-7" }),
    assistant("````html\nLong opener\n```", { id: "answer-8" }),
    assistant("```html\n<div>```", { id: "answer-11" }),
    assistant("```html\ncontent before ```", { id: "answer-12" }),
    assistant("```html\ncontent\n``` trailing", { id: "answer-13" }),
    assistant("```html\ncontent\n```trailing", { id: "answer-17" }),
    assistant("````html\ncontent\n```", { id: "answer-14" }),
    assistant("```html\ncontent\n~~~", { id: "answer-15" }),
    assistant("~~~~html\n<b>x</b>\n~~~", { id: "answer-19" }),
    assistant("~~~html\ncontent\n```", { id: "answer-20" }),
    assistant("text\n\n    indented code\n", { id: "answer-16" }),
    assistant("   ```html\nIndented\n   ```   ", { id: "answer-9" }),
    assistant("```html\n```", { id: "answer-10" }),
  ]);
  assert.deepEqual(
    results.map((result) => {
      assert.ok("source" in result);
      const { kind, label, source } = result;
      return { kind, label, source };
    }),
    [
      { kind: "html", label: "HTML preview", source: "Valid markup" },
      {
        kind: "mermaid",
        label: "Mermaid diagram",
        source: "graph LR; A-->B",
      },
      { kind: "html", label: "HTML preview", source: "Tilde markup" },
      { kind: "html", label: "HTML preview", source: "Long fence\n```" },
      { kind: "html", label: "HTML preview", source: "Indented" },
      { kind: "html", label: "HTML preview", source: "" },
    ],
  );
});

check(
  "adjacent raw HTML groups across spaces and flushes around other Markdown",
  () => {
    const results = conversationResults([
      assistant(
        "<style>p{color:red}</style>\n\n<div>One</div>\n\nordinary text\n\n<div>Two</div>",
      ),
    ]);
    assert.deepEqual(
      results.map((result) => {
        assert.ok("source" in result);
        const { label, kind, source } = result;
        return { label, kind, source };
      }),
      [
        {
          label: "HTML preview",
          kind: "html",
          source: "<style>p{color:red}</style>\n<div>One</div>",
        },
        { label: "HTML preview", kind: "html", source: "<div>Two</div>" },
      ],
    );
  },
);

check(
  "invalid attachment records are skipped while valid names and image flags are preserved",
  () => {
    const results = conversationResults([
      assistant("", {
        assets: [
          // @ts-expect-error A null attachment is invalid and must be ignored.
          null,
          // @ts-expect-error An attachment without its required metadata is invalid and must be ignored.
          {},
          // @ts-expect-error An attachment without its required metadata is invalid and must be ignored.
          { id: "" },
          // @ts-expect-error An attachment ID must be a string; malformed IDs must be ignored.
          { id: 3 },
          // @ts-expect-error A legacy attachment without name and metadata must retain its fallback label.
          { id: "unnamed" },
          // @ts-expect-error Attachment name and image fields have invalid runtime types.
          { id: "non-image", name: 4, image: "true" },
          componentAttachment({ id: "image", name: "Picture", image: true }),
        ],
      }),
    ]);
    assert.deepEqual(
      results.map((result) => {
        assert.ok(result.kind === "asset");
        const { id, label, kind, asset, image } = result;
        return {
          id,
          label,
          kind,
          asset,
          image,
        };
      }),
      [
        {
          id: "answer:result:0",
          label: "Attachment",
          kind: "asset",
          asset: "unnamed",
          image: false,
        },
        {
          id: "answer:result:1",
          label: "Attachment",
          kind: "asset",
          asset: "non-image",
          image: false,
        },
        {
          id: "answer:result:2",
          label: "Picture",
          kind: "asset",
          asset: "image",
          image: true,
        },
      ],
    );
  },
);

check(
  "tool output requires a completed eligible result and valid change evidence",
  () => {
    const results = conversationResults([
      output(
        { type: "fileChange", changes: [{ path: "dir/", diff: "+saved" }] },
        {
          toolStatus: "completed",
        },
      ),
      output(
        {
          type: "fileChange",
          status: "completed",
          changes: [
            null,
            {},
            { path: 3, diff: "+bad path" },
            { path: "missing-diff" },
            { path: "missing-path", diff: 3 },
            { path: "blank", diff: " \n\t " },
            { path: "saved.ts", diff: " +saved\n" },
            { path: "saved.ts", diff: " +saved\n" },
          ],
        },
        { truncated: true },
      ),
    ]);
    assert.equal(results.length, 2);
    assert.deepEqual(
      results.map((result) => {
        assert.ok(result.kind === "patch");
        const { label, kind, paths, source, truncated } = result;
        return {
          label,
          kind,
          paths,
          source,
          truncated,
        };
      }),
      [
        {
          label: "dir/",
          kind: "patch",
          paths: ["dir/"],
          source: "+saved",
          truncated: false,
        },
        {
          label: "saved.ts",
          kind: "patch",
          paths: ["saved.ts"],
          source: " +saved\n",
          truncated: true,
        },
      ],
    );
  },
);

check(
  "malformed and primitive tool payloads, failed statuses, and unknown outputs are ignored",
  () => {
    const results = conversationResults([
      { id: "tool", role: "output", text: "not JSON", title: "Changes" },
      ...["null", "false", "0", '"text"', "[]"].map((text) => ({
        id: "tool",
        role: "output",
        text,
        title: "Changes",
      })),
      assistant(JSON.stringify({ diff: "+not a tool result" }), {
        title: "Changes",
      }),
      output(
        { type: "fileChange", status: "completed", changes: [] },
        {
          toolStatus: "cancelled",
        },
      ),
      output(
        { type: "fileChange", status: "completed", changes: [] },
        {
          toolStatus: "interrupted",
        },
      ),
      output({ diff: "+failure" }, { title: "Changes", toolStatus: "running" }),
      output({ diff: "+failed" }, { title: "Changes", toolStatus: "failed" }),
      output(
        { diff: "+failure" },
        { title: "Changes", toolStatus: "declined" },
      ),
      output(
        { diff: "+failure" },
        { title: "Changes", toolStatus: "cancelled" },
      ),
      output(
        { diff: "+failure" },
        { title: "Changes", toolStatus: "interrupted" },
      ),
      output(
        { status: "inProgress", diff: "+unfinished" },
        { title: "Changes" },
      ),
      output({ status: "failed", diff: "+failed" }, { title: "Changes" }),
      output({ status: "declined", diff: "+declined" }, { title: "Changes" }),
      output({ success: false, diff: "+failed" }, { title: "Changes" }),
      output({ error: "failed", diff: "+failed" }, { title: "Changes" }),
      output({ type: "fileChange", status: "completed", changes: [] }),
      output({
        type: "anotherTool",
        status: "completed",
        changes: [{ path: "untrusted.ts", diff: "+not a file change" }],
      }),
      output({ diff: "+untrusted" }, { title: "Command output" }),
      output({ diff: " \n " }, { title: "Changes" }),
    ]);
    assert.deepEqual(results, []);
  },
);

check(
  "completed turn diffs parse quoted, tabbed, duplicate, and invalid headers",
  () => {
    const diff = [
      '--- "a/name with spaces.txt"\t2025-01-01',
      '+++ "b/name with spaces.txt"\t2025-01-01',
      "--- a/repeated.ts",
      "+++ b/repeated.ts",
      '--- "unterminated',
      "+++ /dev/null",
      "note --- a/not-a-header.ts",
      "--- x/ab/nested.ts",
      "--- a/another.ts",
      "+++ b/another.ts",
      "@@ changes",
    ].join("\n");
    const result = required(
      conversationResults([output({ diff }, { title: "Changes" })])[0],
    );
    assert.ok(result.kind === "patch");
    assert.equal(result.label, "Changes · 4 files");
    assert.deepEqual(result.paths, [
      "name with spaces.txt",
      "repeated.ts",
      "x/ab/nested.ts",
      "another.ts",
    ]);
    assert.equal(result.source, diff);
    assert.equal(result.truncated, false);
  },
);

check(
  "turn diff accepts each native update identity and labels zero or one path",
  () => {
    const onePath = "--- /dev/null\n+++ b/fresh.ts\n@@ +new";
    const noPaths = "@@ no file headers\n+still saved evidence";
    const results = conversationResults([
      output(
        { diff: onePath },
        { title: "turn/diff/updated", truncated: true },
      ),
      output({ diff: noPaths }, { id: "worker:turn/diff/updated" }),
    ]);
    assert.deepEqual(
      results.map((result) => {
        assert.ok(result.kind === "patch");
        const { label, kind, paths, truncated } = result;
        return {
          label,
          kind,
          paths,
          truncated,
        };
      }),
      [
        {
          label: "fresh.ts",
          kind: "patch",
          paths: ["fresh.ts"],
          truncated: true,
        },
        { label: "Changes", kind: "patch", paths: [], truncated: false },
      ],
    );
  },
);
