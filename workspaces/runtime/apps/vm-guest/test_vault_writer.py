"""The vault writer against a fake layr (states as folders) and a fake S3; the reference reader
rebuilds every pushed state."""
from __future__ import annotations

import json
import os
from pathlib import Path
import random
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import vault_writer
from vault_reader import Reader
from vault_writer import Writer


class FakeLayr:
    def __init__(self):
        self.states = {}

    def commit(self, state, files):
        self.states[state] = dict(files)

    def __call__(self, args):
        if args[0] == "export":
            state, folder = args[1], Path(args[2])
            folder.mkdir(parents=True, exist_ok=True)
            wanted = self.states[state]
            for path in list(folder.rglob("*")):
                rel = path.relative_to(folder).as_posix()
                if (path.is_file() or path.is_symlink()) and rel not in wanted and rel != ".layr-export.json":
                    path.unlink()
            for rel, (data, mode) in wanted.items():
                target = folder / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                if os.path.lexists(target):
                    target.unlink()
                if mode == "120000":
                    target.symlink_to(data.decode())
                else:
                    target.write_bytes(data)
                    target.chmod(0o755 if mode == "100755" else 0o644)
            (folder / ".layr-export.json").write_text(json.dumps({"state": state}))
            return ""
        if args[:2] == ["diff", "--name-status"]:
            old, new = self.states[args[2]], self.states[args[3]]
            lines = [f"A\t{p}" for p in new if p not in old] + [f"D\t{p}" for p in old if p not in new]
            lines += [f"M\t{p}" for p in new if p in old and new[p] != old[p]]
            return "\n".join(sorted(lines))
        raise AssertionError(args)


class FakeS3:
    def __init__(self):
        self.objects, self.puts = {}, []

    def put_new(self, key, body):
        if key in self.objects:
            return False
        self.objects[key] = body
        self.puts.append(key)
        return True

    def list(self, prefix):
        return sorted(k for k in self.objects if k.startswith(prefix))


def encrypt(data):
    return b"E" + data[::-1]


def decrypt(data):
    assert data[:1] == b"E"
    return data[1:][::-1]


_random = random.Random(1)
_word = lambda: "".join(_random.choice("abcdefghijklmnopqrstuvwxyz_") for _ in range(_random.randint(4, 12)))
SOURCE = "".join(f"def {_word()}({_word()}, {_word()}):\n    return {_word()}({_word()}) * {i}\n\n"
                 for i in range(400)).encode()


class WriterTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.layr, self.s3 = FakeLayr(), FakeS3()
        self.writer = Writer(Path(self.temp.name) / "vault", "proj", self.layr, encrypt, self.s3, clock=lambda: 1000)

    def reader(self):
        return Reader("proj", list(self.s3.objects), self.s3.objects.__getitem__, decrypt)

    def files(self, reader, seq=None, attempt=None):
        return {p: (reader.read(e), e[0]) for p, e in reader.files(seq, attempt).items()}

    def test_pushes_rebuild_every_state_with_small_deltas(self):
        one = {"app.py": (SOURCE, "100644"), "run.sh": (b"#!/bin/sh\necho hi\n", "100755"),
               "link": (b"app.py", "120000"), "data/empty": (b"", "100644")}
        self.layr.commit("s1", one)
        first = self.writer.push("s1")
        self.assertEqual((first["pushed"], first["seq"]), (True, 1))
        self.assertEqual(self.files(self.reader()), one)

        two = dict(one)
        two["app.py"] = (SOURCE.replace(b" * 7\n", b" * 70\n"), "100644")
        del two["run.sh"]
        two["new.txt"] = (b"new\n", "100644")
        self.layr.commit("s2", two)
        second = self.writer.push("s2")
        self.assertEqual(second["seq"], 2)
        reader = self.reader()
        self.assertEqual(self.files(reader), two)
        entry = reader.files()["app.py"]
        self.assertEqual([frame[5] for frame in entry[3]], ["delta", "full"])
        # The delta of a one-line change is much smaller than the compressed file.
        self.assertLess(entry[3][0][4] * 4, len(vault_writer.COMPRESS(two["app.py"][0])))
        # The first state stays readable, and parts precede their tree.
        first_attempt = sorted(reader.trees[1])[0]
        self.assertEqual(self.files(reader, 1, first_attempt), one)
        tree_keys = [k for k in self.s3.puts if k.endswith(".tree.age")]
        self.assertTrue(all(self.s3.puts.index(k) > self.s3.puts.index(k.replace(".tree.age", ".0000.data.age")) for k in tree_keys))
        self.assertEqual(self.writer.push("s2")["pushed"], False)

    def test_chains_and_trees_start_again_from_full(self):
        files = {"a.txt": (SOURCE, "100644")}
        for n in range(1, 40):
            files = {"a.txt": (files["a.txt"][0] + f"line {n}\n".encode(), "100644")}
            self.layr.commit(f"s{n}", files)
            self.writer.push(f"s{n}")
        reader = self.reader()
        self.assertEqual(self.files(reader), files)
        self.assertLessEqual(len(reader.files()["a.txt"][3]), vault_writer.MAX_DEPTH + 1)
        headers = [reader.tree(seq, sorted(reader.trees[seq])[0])[0] for seq in sorted(reader.trees)]
        self.assertEqual([h["tree"] for h in headers].count("full"), 2)

    def test_a_lost_record_starts_a_full_push_after_the_last_seq(self):
        self.layr.commit("s1", {"a.txt": (SOURCE, "100644")})
        self.writer.push("s1")
        self.writer.record_path.unlink()
        self.layr.commit("s2", {"a.txt": (SOURCE + b"more\n", "100644")})
        result = self.writer.push("s2")
        self.assertEqual(result["seq"], 2)
        reader = self.reader()
        self.assertEqual(reader.files()["a.txt"][3][0][5], "full")
        self.assertEqual(self.files(reader)["a.txt"][0], SOURCE + b"more\n")

    def test_a_failed_tree_put_leaves_the_record_and_a_retry_succeeds(self):
        self.layr.commit("s1", {"a.txt": (SOURCE, "100644")})
        put = self.s3.put_new

        def failing(key, body):
            if key.endswith(".tree.age"):
                raise OSError("lost")
            return put(key, body)
        self.s3.put_new = failing
        with self.assertRaises(OSError):
            self.writer.push("s1")
        self.assertIsNone(self.writer.record())
        self.s3.put_new = put
        self.assertEqual(self.writer.push("s1")["seq"], 1)
        self.assertEqual(self.files(self.reader())["a.txt"][0], SOURCE)


class ConfigureTests(unittest.IsolatedAsyncioTestCase):
    async def test_another_vault_or_key_starts_the_writer_again(self):
        from vault_admin import VaultHandlers
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            (root / "store/projects/app/lines/main").mkdir(parents=True)
            handlers = VaultHandlers(root / "state", root / "store")
            params = {"projectId": "app", "recipient": "age1" + "q" * 58, "bucket": "bucket-one", "region": "eu-central-1",
                      "prefix": "app", "accessKeyId": "AKIAIOSFODNN7EXAMPLE", "secretAccessKey": "s" * 40}
            await handlers.dispatch("1", "vault.configure", params, None)
            record = root / "state/vault/app/writer/writer.json"
            record.parent.mkdir(parents=True)
            record.write_text("{}")
            await handlers.dispatch("2", "vault.configure", {**params, "accessKeyId": "AKIAIOSFODNN7OTHERX"}, None)
            self.assertTrue(record.exists(), "a new writer key keeps the chain")
            await handlers.dispatch("3", "vault.configure", {**params, "recipient": "age1" + "p" * 58}, None)
            self.assertFalse(record.exists(), "a new vault key starts again")
            status = await handlers.dispatch("4", "vault.status", {"projectId": "app"}, None)
            self.assertNotIn("secretAccessKey", json.dumps(status))
            with self.assertRaises(Exception):
                await handlers.dispatch("5", "vault.configure", {**params, "endpoint": "https://evil.example/"}, None)


    async def test_the_periodic_round_pushes_every_project_and_records_the_outcome(self):
        from common import GuestError
        from vault_admin import VaultHandlers
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            handlers = VaultHandlers(root / "state", root / "store")
            for project in ("one", "two"):
                (root / "store/projects" / project / "lines/main").mkdir(parents=True)
                await handlers.dispatch("c-" + project, "vault.configure", {
                    "projectId": project, "recipient": "age1" + "q" * 58, "bucket": "bucket-one", "region": "eu-central-1",
                    "prefix": project, "accessKeyId": "AKIAIOSFODNN7EXAMPLE", "secretAccessKey": "s" * 40}, None)

            def push(project):
                if project == "one":
                    raise OSError("S3 put: HTTP 403")
                return {"pushed": True, "seq": 3}
            handlers.push = push
            await handlers.tick()
            one = await handlers.dispatch("s1", "vault.status", {"projectId": "one"}, None)
            two = await handlers.dispatch("s2", "vault.status", {"projectId": "two"}, None)
            self.assertEqual(one["lastError"], "OSError: S3 put: HTTP 403")
            self.assertNotIn("lastPush", one)
            self.assertIsNone(two["lastError"])
            self.assertEqual(two["lastPush"], two["lastAttempt"])
            with self.assertRaises(GuestError):
                await handlers.dispatch("p1", "vault.push", {"projectId": "one"}, None)


if __name__ == "__main__":
    unittest.main()

