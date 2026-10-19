"""The three-way collect of a host command's changes into a layr line."""
from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
from host_exec_slot_io import collect, run

# `layr show <state>:<path>` from a folder that stands for the held state, and
# `layr status --porcelain --ignored -z` in the line, where the line ignores `target/`. Like layr,
# the status reports only files that exist in the line.
FAKE_LAYR = """#!/bin/sh
if [ "$1" = status ]; then
    [ -d target ] && find target -type f | sort | while read -r f; do printf '!! %s\\0' "$f"; done
    exit 0
fi
[ "$1" = show ] || exit 2
rel=${2#*:}
[ -f "$FAKE_BASE/$rel" ] || { echo "fatal: path '$rel' does not exist" >&2; exit 128; }
cat "$FAKE_BASE/$rel"
"""


class CollectTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.base, self.slot, self.line, tools = root / "base", root / "slot", root / "line", root / "bin"
        for folder in (self.base, self.slot, self.line, tools):
            folder.mkdir()
        (tools / "layr").write_text(FAKE_LAYR)
        (tools / "layr").chmod(0o755)
        self.env = {"PATH": f"{tools}:/usr/local/bin:/usr/bin:/bin", "FAKE_BASE": str(self.base)}

    def tearDown(self):
        self.temp.cleanup()

    def put(self, folder, rel, data):
        path = folder / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data if isinstance(data, bytes) else data.encode())

    def everywhere(self, rel, data):
        for folder in (self.base, self.slot, self.line):
            self.put(folder, rel, data)

    def collect(self, paths):
        return collect({"root": str(self.slot), "line": str(self.line), "held": "abc123",
                        "paths": paths, "env": self.env})

    def test_a_path_only_the_slot_changed_is_copied(self):
        self.everywhere("a.txt", "one\n")
        self.put(self.slot, "a.txt", "two\n")
        self.put(self.slot, "new/b.txt", "new\n")
        report = self.collect(["a.txt", "new/b.txt"])
        self.assertEqual((self.line / "a.txt").read_text(), "two\n")
        self.assertEqual((self.line / "new/b.txt").read_text(), "new\n")
        self.assertEqual((report["copied"], report["conflicts"]), (["a.txt", "new/b.txt"], []))

    def test_a_path_the_slot_deleted_goes_from_an_unchanged_line(self):
        self.everywhere("gone.txt", "x\n")
        (self.slot / "gone.txt").unlink()
        self.assertEqual(self.collect(["gone.txt"])["copied"], ["gone.txt"])
        self.assertFalse((self.line / "gone.txt").exists())

    def test_a_line_change_without_a_slot_change_stays(self):
        self.everywhere("keep.txt", "base\n")
        self.put(self.line, "keep.txt", "line\n")
        self.assertEqual(self.collect(["keep.txt"]), {"copied": [], "conflicts": [], "kept": []})
        self.assertEqual((self.line / "keep.txt").read_text(), "line\n")

    def test_separate_text_changes_on_both_sides_merge(self):
        self.everywhere("m.txt", "a\nb\nc\nd\ne\n")
        self.put(self.line, "m.txt", "A\nb\nc\nd\ne\n")
        self.put(self.slot, "m.txt", "a\nb\nc\nd\nE\n")
        report = self.collect(["m.txt"])
        self.assertEqual((self.line / "m.txt").read_text(), "A\nb\nc\nd\nE\n")
        self.assertEqual((report["copied"], report["conflicts"]), (["m.txt"], []))

    def test_overlapping_text_changes_get_markers(self):
        self.everywhere("c.txt", "x\n")
        self.put(self.line, "c.txt", "line\n")
        self.put(self.slot, "c.txt", "slot\n")
        report = self.collect(["c.txt"])
        text = (self.line / "c.txt").read_text()
        self.assertIn("<<<<<<< line", text)
        self.assertIn(">>>>>>> slot", text)
        self.assertEqual(report["conflicts"], ["c.txt (content: markers written)"])

    def test_binary_changes_on_both_sides_keep_the_line_version(self):
        self.everywhere("b.bin", b"\0base")
        self.put(self.line, "b.bin", b"\0line")
        self.put(self.slot, "b.bin", b"\0slot")
        report = self.collect(["b.bin"])
        self.assertEqual((self.line / "b.bin").read_bytes(), b"\0line")
        self.assertEqual((self.line / "b.bin.slot-conflict").read_bytes(), b"\0slot")
        self.assertEqual(len(report["conflicts"]), 1)

    def test_ignored_build_output_of_the_mac_stays_out_of_the_line(self):
        # A Mac build wrote target/ in a slot whose line had no target cache yet, and changed
        # a binary at a path where the line has its own Linux build.
        self.everywhere("src/main.rs", "fn main() {}\n")
        self.put(self.slot, "src/main.rs", "fn main() { run() }\n")
        self.put(self.slot, "target/release/tool", b"\xcf\xfa\xed\xfe mach-o")
        self.put(self.slot, "target/release/.fingerprint/tool", "mac")
        self.put(self.slot, "target/debug/tool", b"\xcf\xfa\xed\xfe mach-o")
        self.put(self.line, "target/debug/tool", b"\x7fELF linux")
        self.put(self.slot, "gen/new.rs", "// generated\n")
        paths = ["src/main.rs", "target/release/tool", "target/release/.fingerprint/tool",
                 "target/debug/tool", "gen/new.rs"]
        report = self.collect(paths)
        # The placeholders of the ignore check and their folders are gone too.
        self.assertFalse((self.line / "target/release").exists())
        self.assertEqual((self.line / "target/debug/tool").read_bytes(), b"\x7fELF linux")
        self.assertFalse((self.line / "target/debug/tool.slot-conflict").exists())
        self.assertEqual((report["copied"], report["conflicts"]), (["gen/new.rs", "src/main.rs"], []))
        self.assertEqual(report["kept"], ["target/debug/tool", "target/release/.fingerprint/tool",
                                          "target/release/tool"])
        self.assertEqual((self.line / "gen/new.rs").read_text(), "// generated\n")

    def test_an_ignored_path_of_the_held_state_still_returns(self):
        # layr keeps ignored files that are part of a state (such as .env); their changes return.
        self.everywhere("target/keep.txt", "base\n")
        self.put(self.slot, "target/keep.txt", "mac\n")
        report = self.collect(["target/keep.txt"])
        self.assertEqual((report["copied"], report["kept"]), (["target/keep.txt"], []))
        self.assertEqual((self.line / "target/keep.txt").read_text(), "mac\n")

    def test_the_export_marker_and_git_are_never_collected(self):
        self.put(self.slot, ".layr-export.json", "{}")
        self.put(self.slot, ".git/HEAD", "ref")
        self.assertEqual(self.collect([".layr-export.json", ".git/HEAD"]), {"copied": [], "conflicts": [], "kept": []})
        self.assertFalse((self.line / ".layr-export.json").exists())


    def test_the_manifest_leaves_out_mac_only_paths(self):
        self.put(self.slot, "chromium/src/a.cc", "a")
        self.put(self.slot, "chromium/src/out/Default/a.o", "object")
        entries = run({"action": "manifest", "root": str(self.slot), "skip": ["chromium/src/out"]})["entries"]
        self.assertEqual(sorted(entries), ["chromium", "chromium/src", "chromium/src/a.cc"])


if __name__ == "__main__":
    unittest.main()
