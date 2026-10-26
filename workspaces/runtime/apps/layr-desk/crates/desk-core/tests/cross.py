"""The Python writer and reader (python/) on the folders of the Rust writer tests (tests/writer.rs):
a layr whose states are folders, a bucket whose objects are files, and the reversible test
encryption of python/test_vault_writer.py.

    cross.py push <state folder> <prefix> <layr root> <objects> <state>   prints the summary
    cross.py read <prefix> <objects>                                    prints every state
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import shutil
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "python"))
import vault_writer  # noqa: E402
from vault_reader import Reader  # noqa: E402
from vault_writer import Writer  # noqa: E402


def snapshot(folder):
    return {p: vault_writer.content(path) for p, path in vault_writer.walk(folder).items()}


class FolderLayr:
    """`root/<state>/` holds the files of one state."""

    def __init__(self, root):
        self.root = Path(root)

    def __call__(self, args):
        if args[0] == "export":
            state, folder = args[1], Path(args[2])
            shutil.rmtree(folder, ignore_errors=True)
            shutil.copytree(self.root / state, folder, symlinks=True)
            (folder / vault_writer.MARKER).write_text(json.dumps({"state": state}))
            return ""
        if args[:2] == ["diff", "--name-status"]:
            old, new = snapshot(self.root / args[2]), snapshot(self.root / args[3])
            lines = [f"A\t{p}" for p in new if p not in old] + [f"D\t{p}" for p in old if p not in new]
            lines += [f"M\t{p}" for p in new if p in old and new[p] != old[p]]
            return "\n".join(sorted(lines))
        raise AssertionError(args)


class FolderS3:
    def __init__(self, root):
        self.root = Path(root)

    def put_new(self, key, body):
        path = self.root / key
        if path.exists():
            return False
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(body)
        return True

    def list(self, prefix):
        keys = (p.relative_to(self.root).as_posix() for p in self.root.rglob("*") if p.is_file())
        return sorted(k for k in keys if k.startswith(prefix))


def encrypt(data):
    return b"E" + data[::-1]


def decrypt(data):
    assert data[:1] == b"E"
    return data[1:][::-1]


def main(command, *args):
    if command == "push":
        state_dir, prefix, layr_root, objects, head = args
        writer = Writer(state_dir, prefix, FolderLayr(layr_root), encrypt, FolderS3(objects))
        return writer.push(head)
    prefix, objects = args
    s3 = FolderS3(objects)
    reader = Reader(prefix, s3.list(prefix + "/states/"), lambda k: (s3.root / k).read_bytes(), decrypt)
    states = []
    for seq in sorted(reader.trees):
        attempt = sorted(reader.trees[seq])[0]
        header, tree = reader.tree(seq, attempt)
        files = json.loads(tree)["files"]
        states.append({
            "seq": seq, "attempt": attempt, "header": header,
            # The Python writer would write the same bytes for this tree.
            "canonical": vault_writer.canonical(json.loads(tree)) == tree,
            "files": {p: [hashlib.sha256(reader.read(e)).hexdigest(), e[0], [f[5] for f in e[3]]]
                      for p, e in files.items()},
        })
    return {"states": states}


if __name__ == "__main__":
    print(json.dumps(main(*sys.argv[1:])))
