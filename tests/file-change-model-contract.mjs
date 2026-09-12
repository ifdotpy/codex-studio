import assert from "node:assert/strict";
import {
  fileChanges,
  unifiedDiff,
  diffTotals,
  changeLabel,
} from "../web/src/components/fileChangeModel.ts";

let checks = 0;
function check(name, run) {
  run();
  checks++;
  console.log(`PASS ${name}`);
}
function native(diff, type = "update", extra = {}) {
  return fileChanges([{ path: "src/a.ts", kind: { type, ...extra }, diff }])[0];
}
const update =
  "--- a/src/a.ts\n+++ b/src/a.ts\n@@ -3,3 +3,4 @@ function a\n same\n-old\n+new\n+extra\n tail\n";
check(
  "native add and delete preserve raw prefixes and count physical lines",
  () => {
    for (const kind of ["add", "delete"]) {
      const file = native("+literal\n--- header-like\n@@ not a hunk\n", kind);
      assert.equal(file.parsed, true);
      assert.deepEqual(
        file.lines.map((line) => [line.kind, line.text, line.line]),
        [
          [kind, "+literal", 1],
          [kind, "--- header-like", 2],
          [kind, "@@ not a hunk", 3],
        ],
      );
      assert.deepEqual(
        diffTotals([file]),
        kind === "add" ? { added: 3, removed: 0 } : { added: 0, removed: 3 },
      );
    }
  },
);
check(
  "empty files and terminal newline match CLI content.lines semantics",
  () => {
    for (const kind of ["add", "delete"]) {
      for (const [content, count] of [
        ["", 0],
        ["\n", 1],
        ["a", 1],
        ["a\n", 1],
        ["a\n\n", 2],
        ["a\r\nb\r\n", 2],
      ]) {
        const file = native(content, kind);
        assert.equal(file.parsed, true);
        assert.equal(file.lines.length, count);
        assert.equal(file.added + file.removed, count);
      }
    }
  },
);
check(
  "update numbers use old lines for deletion and new lines for context and addition",
  () => {
    const file = native(update);
    assert.equal(file.parsed, true);
    assert.deepEqual(
      file.lines.map((line) => [line.kind, line.text, line.line]),
      [
        ["meta", "@@ -3,3 +3,4 @@ function a", undefined],
        ["context", "same", 3],
        ["delete", "old", 4],
        ["add", "new", 4],
        ["add", "extra", 5],
        ["context", "tail", 6],
      ],
    );
    assert.deepEqual(diffTotals([file]), { added: 2, removed: 1 });
  },
);
check("multiple hunks retain gaps and no-newline metadata", () => {
  const file = native(
    "@@ -1 +1 @@\n-a\n+b\n@@ -10,2 +10,2 @@\n same\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n",
  );
  assert.equal(file.parsed, true);
  assert.deepEqual(
    file.lines.filter((line) => line.line).map((line) => line.line),
    [1, 1, 10, 11, 11],
  );
  assert.equal(file.lines.filter((line) => line.kind === "meta").length, 4);
  assert.deepEqual(diffTotals([file]), { added: 2, removed: 2 });
});
check("native rename suffix is metadata, including rename-only updates", () => {
  const file = native(`${update}\n\nMoved to: src/b.ts`, "update", {
    move_path: "src/b.ts",
  });
  assert.equal(file.parsed, true);
  assert.equal(file.movePath, "src/b.ts");
  assert.deepEqual(diffTotals([file]), { added: 2, removed: 1 });
  assert.equal(changeLabel([file], "completed"), "Edited src/a.ts → src/b.ts");
  const rename = native("\n\nMoved to: other.ts", "update", {
    move_path: "other.ts",
  });
  assert.equal(rename.parsed, true);
  assert.deepEqual(rename.lines, []);
});
check("historical unified fixtures parse without guessing raw data", () => {
  assert.equal(fileChanges([{ path: "a", diff: update }])[0].parsed, true);
  for (const diff of [
    "plain content\n",
    "+added\n-removed\n",
    "",
    "*** Begin Patch\n",
  ]) {
    const file = fileChanges([{ path: "a", diff }])[0];
    assert.equal(file.parsed, false);
    assert.equal(file.source, diff);
    assert.deepEqual(diffTotals([file]), { added: 0, removed: 0 });
  }
  assert.equal(native(update, "binary").parsed, false);
  assert.deepEqual(fileChanges(null), []);
  assert.deepEqual(fileChanges([null, {}, { path: 1 }]), []);
});
check(
  "malformed, clipped, overfull, overlapping, and unsafe ranges retain exact raw fallback",
  () => {
    const invalid = [
      "@@ -1,2 +1,2 @@\n-a\n+b\n",
      "@@ -1 +1 @@\n-a\n+b\n+extra\n",
      "@@ -1 +1 @@\n-a\n+b\n[truncated]\n",
      "@@ -0 +1 @@\n-a\n+b\n",
      "@@ -9007199254740992 +1 @@\n-a\n+b\n",
      "@@ -1,0 +1,0 @@\n",
      "@@ -1 +1 @@\n\\ No newline at end of file\n-a\n+b\n",
      "@@ -1 +1 @@\n-a\n+b\n@@ -1 +1 @@\n-c\n+d\n",
      "--- a/a\n+++ b/a\n",
      "@@ -1 +1 @@\n-a\n+b\n--- malformed\n",
    ];
    for (const source of invalid) {
      const file = native(source);
      assert.equal(file.parsed, false, source);
      assert.deepEqual(file.lines, []);
      assert.equal(file.source, source);
      assert.deepEqual(diffTotals([file]), { added: 0, removed: 0 });
      const aggregate = unifiedDiff(source, "a")[0];
      assert.equal(aggregate.parsed, false, source);
      assert.equal(aggregate.source, source);
    }
  },
);
check("aggregate git diffs infer add, delete, update paths and counts", () => {
  const files = unifiedDiff(
    "diff --git a/a.txt b/a.txt\nnew file mode 100644\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1,2 @@\n+one\n+two\ndiff --git a/b.txt b/b.txt\ndeleted file mode 100644\n--- a/b.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\ndiff --git a/c.txt b/c.txt\nindex 123..456 100644\n--- a/c.txt\n+++ b/c.txt\n@@ -1 +1 @@\n-a\n+b\n",
  );
  assert.deepEqual(
    files.map((file) => [file.path, file.kind, file.parsed]),
    [
      ["a.txt", "add", true],
      ["b.txt", "delete", true],
      ["c.txt", "update", true],
    ],
  );
  assert.deepEqual(diffTotals(files), { added: 3, removed: 2 });
  assert.equal(changeLabel(files, "completed"), "Edited 3 files");
});
check(
  "plain aggregate header pairs do not split header-like hunk contents",
  () => {
    const files = unifiedDiff(
      "--- a/a\n+++ b/a\n@@ -1 +1 @@\n--- raw deleted\n+++ raw added\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-old\n+new\n",
    );
    assert.equal(files.length, 2);
    assert.equal(files[0].parsed, true);
    assert.deepEqual(
      files[0].lines.slice(1).map((line) => line.text),
      ["-- raw deleted", "++ raw added"],
    );
    assert.equal(files[1].path, "b");
    assert.deepEqual(diffTotals(files), { added: 2, removed: 2 });
  },
);
check("git rename-only metadata remains visible", () => {
  const [file] = unifiedDiff(
    "diff --git a/old.ts b/new.ts\nsimilarity index 100%\nrename from old.ts\nrename to new.ts\n",
  );
  assert.equal(file.parsed, true);
  assert.equal(file.path, "old.ts");
  assert.equal(file.movePath, "new.ts");
});
check(
  "empty aggregate additions and deletions retain path and zero counts",
  () => {
    for (const [mode, kind] of [
      ["new", "add"],
      ["deleted", "delete"],
    ]) {
      const [file] = unifiedDiff(
        `diff --git a/empty file b/empty file\n${mode} file mode 100644\n`,
      );
      assert.equal(file.parsed, true);
      assert.equal(file.path, "empty file");
      assert.equal(file.kind, kind);
      assert.deepEqual(diffTotals([file]), { added: 0, removed: 0 });
    }
  },
);
check(
  "aggregate chunks preserve exact CRLF source and malformed chunks do not contribute counts",
  () => {
    const first =
      "diff --git a/a b/a\r\n--- a/a\r\n+++ b/a\r\n@@ -1 +1 @@\r\n-a\r\n+b\r\n";
    const second =
      "diff --git a/b b/b\r\n--- a/b\r\n+++ b/b\r\n@@ -1,2 +1,2 @@\r\n-a\r\n+b\r\n";
    const files = unifiedDiff(first + second);
    assert.deepEqual(
      files.map((file) => file.source),
      [first, second],
    );
    assert.deepEqual(
      files.map((file) => file.parsed),
      [true, false],
    );
    assert.deepEqual(diffTotals(files), { added: 1, removed: 1 });
  },
);
check("add and delete headers reject contradictory hunk ranges", () => {
  for (const source of [
    "--- /dev/null\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
    "--- a/a\n+++ /dev/null\n@@ -1 +1 @@\n-old\n+new\n",
    "diff --git a/a b/a\nnew file mode 100644\n--- /dev/null\n+++ b/a\n",
  ]) {
    const [file] = unifiedDiff(source);
    assert.equal(file.parsed, false);
    assert.deepEqual(diffTotals([file]), { added: 0, removed: 0 });
  }
});
check("only completed status claims a completed edit", () => {
  const files = [native(update)];
  for (const status of ["unknown", "recorded", "", "success", "complete"])
    assert.equal(changeLabel(files, status), "File changes");
  assert.equal(changeLabel(files, "inProgress"), "Applying patch");
  assert.equal(changeLabel(files, "failed"), "Failed to apply patch");
  assert.equal(changeLabel(files, "declined"), "Patch declined");
  assert.equal(changeLabel(files, "cancelled"), "Patch cancelled");
  assert.equal(changeLabel(files, "interrupted"), "Patch interrupted");
  assert.equal(changeLabel([native("", "add")], "completed"), "Added src/a.ts");
  assert.equal(
    changeLabel([native("", "delete")], "completed"),
    "Deleted src/a.ts",
  );
});
check("a final carriage return stays in raw file contents", () => {
  assert.equal(native("a\r", "add").lines[0].text, "a\r");
});
console.log(`PASS ${checks} file change model contracts`);
