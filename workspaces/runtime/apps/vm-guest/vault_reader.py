"""The reference reader of the vault format (docs/vault.md): the tests and the VM checks use it;
layr-desk has the same logic for the browser and the Mac."""
from __future__ import annotations

import json
from pathlib import PurePosixPath

from vault_writer import DECOMPRESS


class Reader:
    """`get(key) -> bytes` reads an object; `decrypt(bytes) -> bytes` opens it; `keys` lists them."""

    def __init__(self, prefix, keys, get, decrypt):
        self.prefix, self.get, self.decrypt = prefix, get, decrypt
        self.trees = {}
        for key in keys:
            name = PurePosixPath(key).name
            if key.startswith(prefix + "/states/") and name.endswith(".tree.age"):
                seq, attempt = name.split(".")[:2]
                self.trees.setdefault(int(seq), []).append(attempt)
        self.cache = {}

    def latest(self):
        seq = max(self.trees)
        return seq, sorted(self.trees[seq])[0]

    def _object(self, key):
        if key not in self.cache:
            self.cache[key] = self.decrypt(self.get(key))
        return self.cache[key]

    def tree(self, seq, attempt):
        """(header, canonical tree bytes) of one pushed state."""
        raw = self._object(f"{self.prefix}/states/{seq:010d}.{attempt}.tree.age")
        line, body = raw.split(b"\n", 1)
        header = json.loads(line)
        if header["tree"] == "full":
            return header, DECOMPRESS(body)
        _, base = self.tree(*header["base"])
        return header, DECOMPRESS(body, base)

    def files(self, seq=None, attempt=None):
        if seq is None:
            seq, attempt = self.latest()
        return json.loads(self.tree(seq, attempt)[1])["files"]

    def read(self, entry):
        """The content of one tree entry: decode its chain from the full frame up."""
        data = None
        for seq, attempt, part, offset, length, kind in reversed(entry[3]):
            blob = self._object(f"{self.prefix}/states/{seq:010d}.{attempt}.{part:04d}.data.age")
            frame = blob[offset:offset + length]
            data = DECOMPRESS(frame, data) if kind == "delta" else DECOMPRESS(frame)
        return data
