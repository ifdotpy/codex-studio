"""Opt-in end-to-end test of the vault writer with real layr, age and zstandard, and a local
fake S3. Run as root on Linux with btrfs and a layr daemon (see test_layr_e2e.py):

    LAYR_E2E_ROOT=/mnt/btrfs/store python3 test_vault_e2e.py
"""
from __future__ import annotations

import http.server
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).parent))
ROOT = os.environ.get("LAYR_E2E_ROOT")


@unittest.skipUnless(ROOT and os.geteuid() == 0 and sys.platform == "linux", "needs LAYR_E2E_ROOT, root and Linux")
class VaultEndToEnd(unittest.IsolatedAsyncioTestCase):
    async def test_two_pushes_rebuild_with_age_and_zstandard(self):
        import vault_writer
        from test_vault_s3 import FakeS3
        from vault_admin import VaultHandlers
        from vault_reader import Reader

        # The guest writes with zstandard (Python 3.12); the reader decodes with the stdlib codec.
        vault_writer._codec[:] = vault_writer.compressor(stdlib=False)
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
        layr = lambda *a, cwd="/": subprocess.run(["layr", *a], cwd=cwd, env=env, check=True, capture_output=True, text=True).stdout
        layr("init", project, "--from", str(source), "--owner", "root", "-q")
        main = Path(ROOT) / "projects" / project / "lines/main"

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeS3)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        handlers = VaultHandlers(temp / "state", Path(ROOT))
        await handlers.dispatch("c", "vault.configure", {
            "projectId": project, "recipient": recipient, "bucket": "test-bucket", "region": "us-east-1",
            "prefix": project, "accessKeyId": "AKIAIOSFODNN7EXAMPLE", "secretAccessKey": "secret-example-0000",
            "endpoint": f"http://127.0.0.1:{server.server_port}"}, None)
        first = await handlers.dispatch("p1", "vault.push", {"projectId": project}, None)
        self.assertEqual((first["pushed"], first["seq"]), (True, 1))

        (main / "app.txt").write_text(lines.replace("line 1000 ", "line one thousand "))
        (main / "new.txt").write_text("new\n")
        (main / "run.sh").unlink()
        layr("add", "-A", cwd=main)
        layr("commit", "-qm", "two", cwd=main)
        second = await handlers.dispatch("p2", "vault.push", {"projectId": project}, None)
        self.assertEqual(second["seq"], 2)

        objects = {k.split("/test-bucket/", 1)[1]: v for k, v in FakeS3.objects.items() if f"/{project}/" in k}
        decrypt = lambda data: subprocess.run(["age", "-d", "-i", str(identity)], input=data, check=True, capture_output=True).stdout
        reader = Reader(project, list(objects), objects.__getitem__, decrypt)
        files = reader.files()
        exported = temp / "export"
        layr("--project", project, "export", "main", str(exported), "-q")
        expected = {}
        for path in exported.rglob("*"):
            rel = path.relative_to(exported).as_posix()
            if rel != ".layr-export.json" and (path.is_file() or path.is_symlink()):
                expected[rel] = os.readlink(path).encode() if path.is_symlink() else path.read_bytes()
        self.assertEqual({p: reader.read(e) for p, e in files.items()}, expected)
        self.assertEqual(files["link"][0], "120000")
        self.assertEqual([frame[5] for frame in files["app.txt"][3]], ["delta", "full"])
        status = await handlers.dispatch("s", "vault.status", {"projectId": project}, None)
        self.assertEqual(status["last"]["seq"], 2)
        self.assertNotIn("secretAccessKey", status)


if __name__ == "__main__":
    unittest.main()
