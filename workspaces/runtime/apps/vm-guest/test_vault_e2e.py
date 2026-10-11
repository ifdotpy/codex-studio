"""Opt-in end-to-end test of the vault broker methods with real layr, the vendored `layr-desk`
built from source, and a local fake S3. Run as root on Linux with btrfs (see test_layr_e2e.py),
with age, age-keygen and Python's zstd (3.14) or the zstandard package for the check:

    LAYR_E2E_ROOT=/mnt/btrfs/store LAYR_E2E_LAYR=/usr/local/bin/layr \
        LAYR_E2E_DESK=/usr/local/bin/layr-desk python3 test_vault_e2e.py
"""
from __future__ import annotations

import base64
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from urllib.parse import parse_qs, unquote, urlsplit
import uuid

sys.path.insert(0, str(Path(__file__).parent))
ROOT = os.environ.get("LAYR_E2E_ROOT")
LAYR = os.environ.get("LAYR_E2E_LAYR", "/usr/local/bin/layr")
DESK = os.environ.get("LAYR_E2E_DESK", "/usr/local/bin/layr-desk")


def decompress(frame, base=None):
    """One zstd frame; a delta frame takes its base as a raw-content dictionary (docs/vault.md)."""
    try:
        from compression import zstd
        return zstd.decompress(frame, zstd_dict=zstd.ZstdDict(base, is_raw=True)) if base else zstd.decompress(frame)
    except ImportError:
        import zstandard
        raw = zstandard.ZstdCompressionDict(base, dict_type=zstandard.DICT_TYPE_RAWCONTENT) if base else None
        return zstandard.ZstdDecompressor(dict_data=raw).decompress(frame)


class Vault:
    """A minimal reader of the pushed objects: the latest tree and each file through its chain."""

    def __init__(self, prefix, objects, decrypt):
        self.prefix, self.objects, self.decrypt = prefix, objects, decrypt

    def get(self, name):
        return self.decrypt(self.objects[f"{self.prefix}/states/{name}"])

    def tree(self, seq, attempt):
        header, body = self.get(f"{seq:010d}.{attempt}.tree.age").split(b"\n", 1)
        header = json.loads(header)
        base = None if header["tree"] == "full" else self.tree(*header["base"])[1]
        return header, decompress(body, base)

    def latest(self):
        names = [key.rsplit("/", 1)[1] for key in self.objects if key.endswith(".tree.age")]
        seq, attempt = max((int(n.split(".")[0]), n.split(".")[1]) for n in names)
        return self.tree(seq, attempt)

    def read(self, entry):
        data = None
        for seq, attempt, part, offset, length, kind in reversed(entry[3]):
            frame = self.get(f"{seq:010d}.{attempt}.{part:04d}.data.age")[offset:offset + length]
            data = decompress(frame, data) if kind == "delta" else decompress(frame)
        return data


class FakeS3(http.server.BaseHTTPRequestHandler):
    """Path-style S3: a put of a new object and ListObjectsV2, as the writer's key may do."""
    objects: dict[str, bytes] = {}

    def log_message(self, *args):
        pass

    def do_PUT(self):
        assert self.headers["authorization"].startswith("AWS4-HMAC-SHA256 ")
        body = self.rfile.read(int(self.headers["content-length"]))
        key = unquote(urlsplit(self.path).path)
        if key in self.objects and self.headers.get("if-none-match") == "*":
            self.send_response(412); self.end_headers(); return
        self.objects[key] = body
        self.send_response(200); self.send_header("content-length", "0"); self.end_headers()

    def do_GET(self):
        assert self.headers["authorization"].startswith("AWS4-HMAC-SHA256 ")
        parts = urlsplit(self.path)
        query = parse_qs(parts.query)
        assert query["list-type"] == ["2"]
        bucket = parts.path.strip("/")
        keys = sorted(k[len(bucket) + 2:] for k in self.objects if k.startswith(f"/{bucket}/{query['prefix'][0]}"))
        body = ("<ListBucketResult>" + "".join(f"<Contents><Key>{k}</Key></Contents>" for k in keys)
                + "</ListBucketResult>").encode()
        self.send_response(200); self.send_header("content-length", str(len(body))); self.end_headers()
        self.wfile.write(body)


@unittest.skipUnless(ROOT and os.geteuid() == 0 and sys.platform == "linux", "needs LAYR_E2E_ROOT, root and Linux")
class VaultEndToEnd(unittest.IsolatedAsyncioTestCase):
    async def test_layr_desk_pushes_two_states_that_rebuild_byte_for_byte(self):
        import vault_admin
        from vault_admin import VaultHandlers

        self.assertTrue(os.access(DESK, os.X_OK), DESK)
        vault_admin.LAYR, vault_admin.LAYR_DESK = LAYR, DESK
        temp = Path(tempfile.mkdtemp(prefix="vault-e2e-"))
        self.addCleanup(shutil.rmtree, temp, True)
        identity = temp / "identity.txt"
        subprocess.run(["age-keygen", "-o", str(identity)], check=True, capture_output=True)
        recipient = subprocess.run(["age-keygen", "-y", str(identity)], check=True, capture_output=True, text=True).stdout.strip()

        project = "vault" + uuid.uuid4().hex[:8]
        source = temp / "src"
        (source / "bin").mkdir(parents=True)
        lines = "".join(f"line {i} {uuid.uuid4().hex}\n" for i in range(2000))
        (source / "app.txt").write_text(lines)
        (source / "bin/data.bin").write_bytes(os.urandom(70000))
        (source / "run.sh").write_text("#!/bin/sh\necho run\n")
        (source / "run.sh").chmod(0o755)
        (source / "link").symlink_to("app.txt")
        env = {**os.environ, "LAYR_ROOT": ROOT}
        layr = lambda *a, cwd="/": subprocess.run([LAYR, *a], cwd=cwd, env=env, check=True, capture_output=True, text=True).stdout
        layr("init", project, "--from", str(source), "--owner", "root", "-q")
        main = Path(ROOT) / "projects" / project / "lines/main"

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeS3)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        handlers = VaultHandlers(temp / "state", Path(ROOT))
        # A signing key is passed through; this layr-desk revision may not use it yet.
        await handlers.dispatch("c", "vault.configure", {
            "projectId": project, "recipient": recipient, "bucket": "test-bucket", "region": "us-east-1",
            "prefix": project, "accessKeyId": "AKIAIOSFODNN7EXAMPLE", "secretAccessKey": "secret-example-0000",
            "signingKey": base64.b64encode(os.urandom(32)).decode(),
            "endpoint": f"http://127.0.0.1:{server.server_port}"}, None)
        first = await handlers.dispatch("p1", "vault.push", {"projectId": project}, None)
        self.assertEqual((first["pushed"], first["seq"]), (True, 1))

        (main / "app.txt").write_text(lines.replace("line 1000 ", "line one thousand "))
        (main / "new.txt").write_text("new\n")
        (main / "run.sh").unlink()
        layr("add", "-A", cwd=main)
        layr("commit", "-qm", "two", cwd=main)
        second = await handlers.dispatch("p2", "vault.push", {"projectId": project}, None)
        self.assertEqual((second["pushed"], second["seq"]), (True, 2))
        third = await handlers.dispatch("p3", "vault.push", {"projectId": project}, None)
        self.assertEqual((third["pushed"], third["seq"]), (False, 2))

        objects = {k.split("/test-bucket/", 1)[1]: v for k, v in FakeS3.objects.items() if f"/{project}/" in k}
        decrypt = lambda data: subprocess.run(["age", "-d", "-i", str(identity)], input=data, check=True, capture_output=True).stdout
        vault = Vault(project, objects, decrypt)
        header, tree = vault.latest()
        self.assertEqual((header["seq"], header["tree"]), (2, "delta"))
        self.assertEqual(header["state"], layr("--project", project, "rev-parse", "main").strip())
        files = json.loads(tree)["files"]
        exported = temp / "export"
        layr("--project", project, "export", "main", str(exported), "-q")
        expected = {}
        for path in exported.rglob("*"):
            rel = path.relative_to(exported).as_posix()
            if rel != ".layr-export.json" and (path.is_file() or path.is_symlink()):
                expected[rel] = os.readlink(path).encode() if path.is_symlink() else path.read_bytes()
        self.assertEqual({p: vault.read(e) for p, e in files.items()}, expected)
        self.assertEqual(files["link"][0], "120000")
        self.assertEqual([frame[5] for frame in files["app.txt"][3]], ["delta", "full"])
        status = await handlers.dispatch("s", "vault.status", {"projectId": project}, None)
        self.assertEqual(status["last"]["seq"], 2)
        self.assertIsNone(status["lastError"])
        self.assertLessEqual(status["lastPush"], status["lastAttempt"])
        self.assertNotIn("secret-example-0000", json.dumps(status))


if __name__ == "__main__":
    unittest.main()
