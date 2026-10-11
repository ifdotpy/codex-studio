"""The vault writer: one push puts the changes of a project's main head into S3 (docs/vault.md).

The writer holds only the vault recipient. Two export folders take turns: one holds the last
pushed state (the base of deltas), the other becomes the new state.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import stat
import time

PART_LIMIT = 16 * 1024 * 1024
FULL_TREE_EVERY = 32
MAX_DEPTH = 16
DELTA_LIMIT = 32 * 1024 * 1024
MARKER = ".layr-export.json"
LEVEL = 3


def compressor(stdlib=True):
    """(compress(data, base=None) -> frame, decompress(frame, base=None) -> data) with zstd; a
    base is a raw-content dictionary, as `zstd --patch-from` and the browser reader use it.
    The guest (Python 3.12) uses the zstandard package; Python 3.14 has compression.zstd."""
    try:
        if not stdlib:
            raise ImportError("zstandard asked")
        from compression import zstd

        def compress(data, base=None):
            if base:
                return zstd.compress(data, LEVEL, zstd_dict=zstd.ZstdDict(base, is_raw=True))
            return zstd.compress(data, LEVEL)

        def decompress(frame, base=None):
            if base:
                return zstd.decompress(frame, zstd_dict=zstd.ZstdDict(base, is_raw=True))
            return zstd.decompress(frame)
        return compress, decompress
    except ImportError:
        import zstandard

        def raw(base):
            return zstandard.ZstdCompressionDict(base, dict_type=zstandard.DICT_TYPE_RAWCONTENT)

        def compress(data, base=None):
            options = {"level": LEVEL, "write_content_size": True}
            if base:
                options["dict_data"] = raw(base)
            return zstandard.ZstdCompressor(**options).compress(data)

        def decompress(frame, base=None):
            return zstandard.ZstdDecompressor(dict_data=raw(base) if base else None).decompress(frame)
        return compress, decompress


_codec = []


def COMPRESS(data, base=None):
    """zstd at LEVEL; the codec loads on first use, so a guest without it still starts."""
    if not _codec:
        _codec.extend(compressor())
    return _codec[0](data, base)


def DECOMPRESS(frame, base=None):
    if not _codec:
        _codec.extend(compressor())
    return _codec[1](frame, base)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def key_of(prefix, seq, attempt, part=None):
    name = f"{seq:010d}.{attempt}." + ("tree" if part is None else f"{part:04d}.data")
    return f"{prefix}/states/{name}.age"


def walk(root):
    """Every file and link below `root` (relative paths), without the export marker."""
    found = {}
    for directory, folders, names in os.walk(root):
        base = Path(directory)
        for name in names + [f for f in folders if (base / f).is_symlink()]:
            path = base / name
            rel = path.relative_to(root).as_posix()
            if rel != MARKER:
                found[rel] = path
        folders[:] = [f for f in folders if not (base / f).is_symlink()]
    return found


def content(path):
    """The bytes and mode of a file or link."""
    meta = os.lstat(path)
    if stat.S_ISLNK(meta.st_mode):
        return os.readlink(path).encode(), "120000"
    return path.read_bytes(), "100755" if meta.st_mode & 0o111 else "100644"


class Writer:
    """`layr(args) -> str` runs layr for the project as root; `encrypt(bytes) -> bytes` encrypts to
    the vault recipient; `s3` has put_new(key, body) and list(prefix)."""

    def __init__(self, state_dir, prefix, layr, encrypt, s3, *, clock=time.time):
        self.dir, self.prefix, self.layr, self.encrypt, self.s3, self.clock = Path(state_dir), prefix, layr, encrypt, s3, clock
        self.dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.record_path = self.dir / "writer.json"

    def record(self):
        if self.record_path.exists():
            return json.loads(self.record_path.read_text())
        return None

    def save(self, record):
        temporary = self.record_path.with_suffix(".tmp")
        temporary.write_text(json.dumps(record))
        os.replace(temporary, self.record_path)

    def next_seq_without_record(self):
        seqs = [int(Path(k).name.split(".")[0]) for k in self.s3.list(self.prefix + "/states/") if k.endswith(".tree.age")]
        return max(seqs, default=0) + 1

    def push(self, head):
        """Push the state `head` (a layr state id). Returns a summary."""
        record = self.record()
        if record and record["state"] == head:
            return {"pushed": False, "seq": record["seq"], "state": head}
        seq = record["seq"] + 1 if record else self.next_seq_without_record()
        attempt = secrets.token_hex(4)
        side = "a" if not record or record["side"] == "b" else "b"
        new_dir, base_dir = self.dir / side, self.dir / ("b" if side == "a" else "a")
        old_tree = json.loads((self.dir / "tree.json").read_text()) if record else {"files": {}}
        if not record:
            shutil.rmtree(new_dir, ignore_errors=True)
        self.layr(["export", head, str(new_dir), "-q"])
        new_files = walk(new_dir)
        if record:
            changed = set()
            for line in self.layr(["diff", "--name-status", record["state"], head]).splitlines():
                parts = line.split("\t")
                changed.update(parts[1:])
            changed = {p for p in changed if p in new_files or p in old_tree["files"]}
        else:
            changed = set(new_files)
        files = {p: v for p, v in old_tree["files"].items() if p in new_files}
        parts, current, frames = [], bytearray(), 0

        def add(frame):
            nonlocal current
            if current and len(current) + len(frame) > PART_LIMIT:
                parts.append(bytes(current)); current = bytearray()
            offset = len(current)
            current.extend(frame)
            return [seq, attempt, len(parts), offset, len(frame)]

        for path in sorted(changed):
            if path not in new_files:
                files.pop(path, None)
                continue
            data, mode = content(new_files[path])
            digest = hashlib.sha256(data).hexdigest()
            previous = old_tree["files"].get(path)
            if previous and previous[2] == digest:
                files[path] = [mode, len(data), digest, previous[3]]
                continue
            frame, chain = COMPRESS(data), []
            if previous and record and len(previous[3]) <= MAX_DEPTH and len(data) <= DELTA_LIMIT:
                base_path = base_dir / path
                if os.path.lexists(base_path):
                    base, _ = content(base_path)
                    if hashlib.sha256(base).hexdigest() == previous[2] and base:
                        delta = COMPRESS(data, base)
                        if len(delta) < len(frame):
                            frame, chain = delta, previous[3]
            location = add(frame)
            files[path] = [mode, len(data), digest, [location + ["delta" if chain else "full"]] + chain]
            frames += 1
        if current or not parts:
            parts.append(bytes(current))
        tree = {"files": files}
        full = not record or record["since_full"] + 1 >= FULL_TREE_EVERY
        body = COMPRESS(canonical(tree)) if full else COMPRESS(canonical(tree), canonical(old_tree))
        header = {"v": 1, "seq": seq, "attempt": attempt, "state": head, "time": int(self.clock()),
                  "tree": "full" if full else "delta", "base": None if full else [record["seq"], record["attempt"]],
                  "parts": len(parts)}
        for number, part in enumerate(parts):
            self.s3.put_new(key_of(self.prefix, seq, attempt, number), self.encrypt(part))
        if not self.s3.put_new(key_of(self.prefix, seq, attempt), self.encrypt(canonical(header) + b"\n" + body)):
            raise RuntimeError("The tree object exists before its first put")
        (self.dir / "tree.json").write_text(canonical(tree).decode())
        self.save({"seq": seq, "attempt": attempt, "state": head, "side": side,
                   "since_full": 0 if full else record["since_full"] + 1})
        return {"pushed": True, "seq": seq, "state": head, "files": frames, "parts": len(parts),
                "bytes": sum(len(p) for p in parts) + len(body)}
