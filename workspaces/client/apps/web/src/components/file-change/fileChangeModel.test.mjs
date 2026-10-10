import assert from "node:assert/strict";
import {
  fileChanges,
  unifiedDiff,
  diffTotals,
  changeLabel,
  claudeEditPatch,
} from "./fileChangeModel.ts";

import { it } from "vitest";

function check(name, run) {
  it(name, run);
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
  assert.equal(file.kind, "update");
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

check("file mode metadata preserves its declared change kind", () => {
  const [addition] = unifiedDiff(
    "diff --git a/empty b/empty\nnew file mode 100644\n",
  );
  assert.equal(addition.kind, "add");
  const [deletion] = unifiedDiff(
    "diff --git a/empty b/empty\ndeleted file mode 100644\n",
  );
  assert.equal(deletion.kind, "delete");
  const [noMode] = unifiedDiff(
    "diff --git a/empty b/empty\nsimilarity index 100%\n",
  );
  assert.equal(noMode.parsed, false);
});
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
check("a Claude Edit diff is rebuilt from its multi-line input", () => {
  const args = {
    file_path: "/r/scripts/a.py",
    old_string: "one\n    two",
    new_string: "one\n    two\n    # note\n    three",
  };
  const saved = "-" + args.old_string + "\n+" + args.new_string;
  const [file] = fileChanges(
    [{ path: args.file_path, kind: { type: "update" }, diff: saved }],
    args,
  );
  assert.equal(file.parsed, true);
  assert.deepEqual(
    file.lines
      .filter((line) => line.kind !== "meta")
      .map((line) => [line.kind, line.text]),
    [
      ["delete", "one"],
      ["delete", "    two"],
      ["add", "one"],
      ["add", "    two"],
      ["add", "    # note"],
      ["add", "    three"],
    ],
  );
  assert.deepEqual([file.added, file.removed], [4, 2]);
});

check(
  "Claude Edit patches preserve empty sides and strip one terminal newline",
  () => {
    assert.equal(
      claudeEditPatch({ old_string: "", new_string: "a\n\n" }),
      "@@ -0,0 +1,2 @@\n+a\n+",
    );
    assert.equal(
      claudeEditPatch({ old_string: "old\n", new_string: "" }),
      "@@ -1,1 +0,0 @@\n-old",
    );
    for (const args of [
      null,
      1,
      {},
      { old_string: 1, new_string: "x" },
      { old_string: "x", new_string: null },
      { old_string: "", new_string: "" },
    ])
      assert.equal(claudeEditPatch(args), null);
    const callable = Object.assign(() => {}, {
      old_string: "old",
      new_string: "new",
    });
    assert.equal(claudeEditPatch(callable), null);
  },
);

check(
  "quoted, tab-delimited and requested paths retain their recorded identity",
  () => {
    const quoted = unifiedDiff(
      '--- "a/space\\tname.ts"\told\n+++ "b/space\\tname.ts"\tnew\n@@ -1 +1 @@\n-old\n+new\n',
    )[0];
    assert.equal(quoted.parsed, true);
    assert.equal(quoted.path, "space\tname.ts");
    const unquotedJsonToken = unifiedDiff(
      "--- true\n+++ true\n@@ -1 +1 @@\n-old\n+new\n",
    )[0];
    assert.equal(unquotedJsonToken.parsed, true);
    assert.equal(unquotedJsonToken.path, "true");
    assert.equal(
      unifiedDiff(
        "--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\n",
        "display.ts",
      )[0].path,
      "display.ts",
    );
    const malformed = unifiedDiff(
      '--- "a/bad\\q.ts"\n+++ b/good.ts\n@@ -1 +1 @@\n-a\n+b\n',
    )[0];
    assert.equal(malformed.parsed, true);
    assert.equal(malformed.path, '"a/bad\\q.ts"');
  },
);

check(
  "hunk validation rejects unsafe totals, zero starts, repeated ranges and orphan markers",
  () => {
    for (const patch of [
      "@@ -9007199254740991,1 +1 @@\n-a\n+b\n",
      "@@ -0,1 +1 @@\n-a\n+b\n",
      "@@ -1 +0,1 @@\n-a\n+b\n",
      "@@ -1,0 +1,0 @@\n",
      "--- /dev/null\n+++ b/a\n@@ -0,1 +1 @@\n+a\n",
      "--- /dev/null\n+++ b/a\n@@ -1,0 +1 @@\n+a\n",
      "--- a/a\n+++ /dev/null\n@@ -1 +0,1 @@\n-a\n",
      "--- a/a\n+++ /dev/null\n@@ -1 +1,0 @@\n-a\n",
      "@@ -1 +9007199254740991,1 @@\n-a\n+b\n",
      "@@ -1 +1 @@\n\\ No newline at end of file\n-a\n+b\n",
      "@@ -1 +1 @@\n-a\n\\ No newline at end of file\n\\ No newline at end of file\n+b\n",
      "@@ -1 +1 @@\n-a\n+b\n@@ -1 +1 @@\n-c\n+d\n",
      "@@ -1 +1 @@\n-a\n+b\n@@ -1 +2 @@\n-c\n+d\n",
      "@@ -1 +1 @@\n-a\n+b\n@@ -2 +1 @@\n-c\n+d\n",
      "@@ -1 +1 @@\n-a\n+b\n@@ -2,2 +2 @@\n-c\n+d\n",
      "@@ -1 +1,2 @@\n-a\nwrong\n",
      "@@ -1,2 +1 @@\nwrong\n+b\n",
      "@@ -1 +1 @@\n-a\n+b\ntrailing garbage\n",
      "not a hunk @@ -1 +1 @@\n-a\n+b\n",
    ]) {
      const [file] = unifiedDiff(patch);
      assert.equal(file.parsed, false, patch);
      assert.deepEqual(file.lines, []);
      assert.deepEqual(diffTotals([file]), { added: 0, removed: 0 });
    }
  },
);

check(
  "hunk accounting accepts multi-digit ranges and unified context lines",
  () => {
    const body = Array.from(
      { length: 14 },
      (_, index) =>
        `${index === 1 ? "-old" : index === 13 ? "+new" : ` line${index}`}`,
    ).join("\n");
    const patch = `--- a/long.ts\n+++ b/long.ts\n@@ -20,13 +20,13 @@ section\n${body}\n`;
    const [file] = unifiedDiff(patch);
    assert.equal(file.parsed, true);
    assert.deepEqual([file.added, file.removed], [1, 1]);
    assert.equal(file.lines[1].kind, "context");
    assert.equal(file.lines[1].line, 20);
    assert.equal(file.lines.at(-1).kind, "add");
  },
);

check(
  "headerless hunks remain updates and hunk ranges allow only ordered adjacency",
  () => {
    const [headerless] = unifiedDiff("@@ -1 +1 @@\n-old\n+new\n");
    assert.equal(headerless.parsed, true);
    assert.equal(headerless.kind, "update");
    const adjacent = unifiedDiff(
      "@@ -1 +1 @@\n-old\n+new\n@@ -2 +2 @@\n-old2\n+new2\n",
    )[0];
    assert.equal(adjacent.parsed, true);
    assert.equal(
      adjacent.lines.filter(({ kind }) => kind === "meta").length,
      2,
    );
    for (const patch of [
      "@@ -1 +1 @@\n-old\n+new\n@@ -1 +2 @@\n-old2\n+new2\n",
      "@@ -1 +1 @@\n-old\n+new\n@@ -2 +1 @@\n-old2\n+new2\n",
    ])
      assert.equal(unifiedDiff(patch)[0].parsed, false);
  },
);

check(
  "diff metadata is recognized only at line starts and requires a path pair",
  () => {
    const [embedded] = unifiedDiff("junk index bogus\n");
    assert.equal(embedded.parsed, false);
    const [incompletePair] = unifiedDiff("--- a/a.ts\n");
    assert.equal(incompletePair.parsed, false);
    for (const patch of [
      "prefix --- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\n",
      "--- a/a.ts\nwrong header\n@@ -1 +1 @@\n-a\n+b\n",
    ])
      assert.equal(unifiedDiff(patch)[0].parsed, false);
    const [missingPath] = unifiedDiff(
      "--- \n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\n",
    );
    assert.equal(missingPath.parsed, false);
    const [junk] = unifiedDiff(
      "junk before diff\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-a\n+b\n",
    );
    assert.equal(junk.parsed, false);
  },
);

check(
  "diff headers split only at complete file boundaries and preserve raw endings",
  () => {
    assert.deepEqual(unifiedDiff(""), []);
    const first =
      "--- a/a\n+++ b/a\n@@ -1 +1 @@\n--- removed header\n+++ added header\n";
    const second = "--- a/b\n+++ b/b\n@@ -1 +1 @@\n-old\n+new";
    const files = unifiedDiff(first + second);
    assert.equal(files.length, 2);
    assert.equal(files[0].parsed, true);
    assert.equal(files[1].parsed, true);
    assert.equal(files[0].source, first);
    assert.equal(files[1].source, second);
    assert.deepEqual(
      files.map((file) => file.path),
      ["a", "b"],
    );
  },
);

check(
  "aggregate hunk counters preserve context and unequal ranges across files",
  () => {
    const first =
      "--- a/context.ts\n+++ b/context.ts\n@@ -3,2 +3,3 @@\n same\n-old\n+new\n+extra\n";
    const second =
      "--- a/next.ts\n+++ b/next.ts\n@@ -1 +1 @@\n-before\n+after\n";
    const files = unifiedDiff(first + second);
    assert.deepEqual(
      files.map(({ path, parsed, source }) => [path, parsed, source]),
      [
        ["context.ts", true, first],
        ["next.ts", true, second],
      ],
    );
    assert.deepEqual(
      unifiedDiff(first + second, "display.ts").map(({ path }) => path),
      ["context.ts", "next.ts"],
    );

    const incomplete = unifiedDiff(
      "--- a/a\n+++ b/a\n@@ -1 +1,2 @@\n-old\n+new\n--- raw\n+++ raw\n+extra\ndiff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-old\n+new\n",
    );
    assert.equal(incomplete.length, 2);
    assert.equal(incomplete[0].parsed, false);
    assert.equal(incomplete[1].path, "b");
  },
);

check(
  "header paths strip only leading Git prefixes and partial renames stay invalid",
  () => {
    const [nested] = unifiedDiff(
      "--- c/a/file.ts\n+++ d/b/file.ts\n@@ -1 +1 @@\n-old\n+new\n",
    );
    assert.equal(nested.path, "c/a/file.ts");
    for (const metadata of ["rename from old.ts\n", "rename to new.ts\n"]) {
      const [file] = unifiedDiff(`diff --git a/old.ts b/new.ts\n${metadata}`);
      assert.equal(file.parsed, false);
    }
  },
);

check(
  "metadata after a completed hunk does not validate a malformed patch",
  () => {
    const [file] = unifiedDiff(
      "@@ -1 +1 @@\n-old\n+new\nindex trailing metadata\n",
    );
    assert.equal(file.parsed, false);

    const [unpaired] = unifiedDiff(
      "@@ -1 +1 @@\n-old\n+new\n+orphan\n--- a/next.ts\n+++ b/next.ts\n@@ -1 +1 @@\n-before\n+after\n",
    );
    assert.equal(unpaired.parsed, false);
  },
);

check("malformed hunk body prefixes do not count as additions", () => {
  const source =
    "@@ -0,0 +1,1 @@\n?not an addition\ndiff --git a/next b/next\n--- a/next\n+++ b/next\n@@ -1 +1 @@\n-before\n+after\n";
  const [file] = unifiedDiff(source);
  const files = unifiedDiff(source);
  assert.equal(file.parsed, false);
  assert.deepEqual([file.added, file.removed], [0, 0]);
  assert.equal(files.length, 2);
  assert.equal(files[1].parsed, true);
});

check("header like deletion content stays inside a one sided hunk", () => {
  const patch = "@@ -1,1 +0,0 @@\n--- text\n";
  const [file] = unifiedDiff(patch);
  assert.equal(file.parsed, true);
  assert.equal(file.source, patch);
  assert.deepEqual(file.lines.at(-1), {
    kind: "delete",
    text: "-- text",
    line: 1,
  });
});

check(
  "incomplete old ranges do not split on header like deletion content",
  () => {
    const files = unifiedDiff(
      "diff --git a/first b/first\n--- a/first\n+++ b/first\n@@ -1,2 +1,1 @@\n context\n--- a/next\n+++ b/next\n@@ -1 +1 @@\n-before\n+after\ndiff --git a/third b/third\n--- a/third\n+++ b/third\n@@ -1 +1 @@\n-before\n+after\n",
    );
    assert.deepEqual(
      files.map(({ path }) => path),
      ["first", "third"],
    );
  },
);

check("hunk validation rejects a one sided undercount", () => {
  const [file] = unifiedDiff("@@ -1,1 +1,2 @@\n-old\n+new\n");
  assert.equal(file.parsed, false);
});

check("hunk headers reject carriage returns inside their trailing text", () => {
  const [file] = unifiedDiff(
    "--- a/x\n+++ b/x\n@@ -1 +1 @@ a\rb\n-old\n+new\n",
  );
  assert.equal(file.parsed, false);
  assert.deepEqual([file.added, file.removed], [0, 0]);
});

check("stray additions do not hide the next file header", () => {
  const second = "--- a/next.ts\n+++ b/next.ts\n@@ -1 +1 @@\n-before\n+after\n";
  const files = unifiedDiff(
    "--- a/first.ts\n+++ b/first.ts\n@@ -1 +1 @@\n-old\n+new\n+orphan\n" +
      second,
  );
  assert.equal(files.length, 2);
  assert.equal(files[1].path, "next.ts");
});

check("file boundaries require both patch header prefixes", () => {
  const file = (path) =>
    `--- a/${path}\n+++ b/${path}\n@@ -1 +1 @@\n-before\n+after\n`;
  const completePair = unifiedDiff(
    file("first") + "garbage\n+++ raw\n" + file("second"),
  );
  assert.deepEqual(
    completePair.map(({ path }) => path),
    ["first", "second"],
  );
  const incompletePair = unifiedDiff(
    file("first") + "--- raw\nnot plus\n" + file("second"),
  );
  assert.deepEqual(
    incompletePair.map(({ path }) => path),
    ["first", "second"],
  );
});

check("unrecognized hunk body lines do not complete addition ranges", () => {
  const patch =
    "--- a/first\n+++ b/first\n@@ -1,0 +1,1 @@\n?not an addition\n--- a/next\n+++ b/next\n@@ -1 +1 @@\n-before\n+after\n";
  const files = unifiedDiff(patch);
  assert.equal(files.length, 1);
  assert.equal(files[0].parsed, false);
});

check("git metadata path inference rejects a trailing suffix", () => {
  const [file] = unifiedDiff(
    "diff --git a/claimed.ts b/claimed.ts trailing\nnew file mode 100644\n",
  );
  assert.equal(file.parsed, false);
  assert.equal(file.path, "");
});

check("rename metadata keeps a diff like source path literal", () => {
  const [file] = unifiedDiff("rename from diff --git a/k b/k\nrename to y\n");
  assert.equal(file.parsed, true);
  assert.equal(file.path, "diff --git a/k b/k");
});

check("unquoted leading space before a quoted path remains literal", () => {
  const [file] = unifiedDiff(
    '---  "a/x"\n+++  "b/x"\n@@ -1 +1 @@\n-old\n+new\n',
  );
  assert.equal(file.parsed, true);
  assert.equal(file.path, ' "a/x"');
});

check("rename header pairs preserve an inferred add kind without hunks", () => {
  const [file] = unifiedDiff(
    "diff --git a/x b/y\nrename from x\nrename to y\n--- /dev/null\n+++ b/y\n",
  );
  assert.equal(file.parsed, true);
  assert.equal(file.kind, "add");
});

check("metadata substrings do not bypass line-start validation", () => {
  const patch =
    "garbage index 123\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-old\n+new\n";
  assert.equal(unifiedDiff(patch)[0].parsed, false);
});

check(
  "file change normalization handles missing diffs, invalid kinds and move spellings",
  () => {
    const files = fileChanges([
      { path: "missing.ts", kind: "add" },
      { path: "unknown.ts", kind: "mystery", diff: "raw" },
      { path: "plain.ts", diff: "raw" },
      {
        path: "rename.ts",
        kind: { type: "update", movePath: "renamed.ts" },
        diff: "",
      },
      {
        path: "suffix.ts",
        kind: { type: "update", move_path: "target.ts" },
        diff: `${update}\n\nMoved to: target.ts`,
      },
      {
        path: "wrong.ts",
        kind: { type: "update", move_path: 2 },
        diff: update,
      },
    ]);
    assert.deepEqual(
      files.map(({ path, kind, parsed, movePath, source }) => [
        path,
        kind,
        parsed,
        movePath,
        source,
      ]),
      [
        ["missing.ts", "add", false, undefined, ""],
        ["unknown.ts", "unknown", false, undefined, "raw"],
        ["plain.ts", "unknown", false, undefined, "raw"],
        ["rename.ts", "update", true, "renamed.ts", ""],
        [
          "suffix.ts",
          "update",
          true,
          "target.ts",
          `${update}\n\nMoved to: target.ts`,
        ],
        ["wrong.ts", "update", true, undefined, update],
      ],
    );
    const unchanged = fileChanges(
      [
        { path: "one.ts", kind: { type: "update" }, diff: update },
        { path: "two.ts", kind: { type: "update" }, diff: update },
      ],
      { old_string: "old", new_string: "new" },
    );
    assert.equal(unchanged.length, 2);
    assert.deepEqual(
      unchanged.map(({ path, added, removed }) => [path, added, removed]),
      [
        ["one.ts", 2, 1],
        ["two.ts", 2, 1],
      ],
    );
    const callableItem = Object.assign(() => {}, {
      path: "callable.ts",
      kind: "update",
      diff: update,
    });
    assert.deepEqual(fileChanges([callableItem]), []);
    const malformedMove = native("not a patch", "update", {
      move_path: "target.ts",
    });
    assert.equal(malformedMove.parsed, false);
    const addedWithoutKind = fileChanges([
      {
        path: "new.ts",
        diff: "--- /dev/null\n+++ b/new.ts\n@@ -0,0 +1 @@\n+new\n",
      },
    ])[0];
    assert.equal(addedWithoutKind.kind, "add");
    assert.equal(addedWithoutKind.parsed, true);
  },
);

check(
  "completed labels distinguish normalized statuses, file counts and move paths",
  () => {
    const one = native(update);
    for (const status of [
      "in_progress",
      "in-progress",
      "in progress",
      "RUNNING",
      "pending",
      "started",
    ])
      assert.equal(changeLabel([one], status), "Applying patch");
    for (const status of ["failed", "ERROR"])
      assert.equal(changeLabel([one], status), "Failed to apply patch");
    for (const status of ["declined", "rejected"])
      assert.equal(changeLabel([one], status), "Patch declined");
    for (const status of ["cancelled", "canceled"])
      assert.equal(changeLabel([one], status), "Patch cancelled");
    assert.equal(changeLabel([one], "interrupted"), "Patch interrupted");
    assert.equal(changeLabel([], "completed"), "Edited files");
    assert.equal(changeLabel([one, one], "completed"), "Edited 2 files");
  },
);
