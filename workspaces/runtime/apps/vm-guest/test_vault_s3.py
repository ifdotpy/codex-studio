"""SigV4 signatures against the worked examples of the S3 documentation, and the put and
list calls against a local fake S3."""
from __future__ import annotations

import datetime
import http.server
from pathlib import Path
import sys
import threading
import unittest

sys.path.insert(0, str(Path(__file__).parent))
from vault_s3 import S3, sign

KEY_ID, SECRET = "AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
NOW = datetime.datetime(2013, 5, 24, tzinfo=datetime.timezone.utc)
HOST = "examplebucket.s3.amazonaws.com"


def signature(*args, **kwargs):
    return sign(*args, key_id=KEY_ID, secret=SECRET, region="us-east-1", now=NOW, **kwargs)["authorization"].rsplit("=", 1)[1]


class SignatureTests(unittest.TestCase):
    """docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html"""

    def test_get_object(self):
        self.assertEqual(signature("GET", HOST, "/test.txt", {}, {"range": "bytes=0-9"},
                                   "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
                         "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41")

    def test_put_object(self):
        self.assertEqual(signature("PUT", HOST, "/test$file.text", {},
                                   {"date": "Fri, 24 May 2013 00:00:00 GMT", "x-amz-storage-class": "REDUCED_REDUNDANCY"},
                                   "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"),
                         "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd")

    def test_list_objects(self):
        self.assertEqual(signature("GET", HOST, "/", {"max-keys": "2", "prefix": "J"}, {},
                                   "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
                         "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7")


class FakeS3(http.server.BaseHTTPRequestHandler):
    objects: dict = {}

    def log_message(self, *args):
        pass

    def do_PUT(self):
        assert self.headers["authorization"].startswith("AWS4-HMAC-SHA256 ")
        body = self.rfile.read(int(self.headers["content-length"]))
        if self.path in self.objects and self.headers.get("if-none-match") == "*":
            self.send_response(412); self.end_headers(); return
        self.objects[self.path] = body
        self.send_response(200); self.end_headers()

    def do_GET(self):
        from urllib.parse import parse_qs, urlsplit, unquote
        parts = urlsplit(self.path)
        prefix = parse_qs(parts.query)["prefix"][0]
        bucket = parts.path.strip("/")
        keys = sorted(unquote(k)[len(bucket) + 2:] for k in self.objects if unquote(k).startswith(f"/{bucket}/{prefix}"))
        body = ("<ListBucketResult>" + "".join(f"<Contents><Key>{k}</Key></Contents>" for k in keys)
                + "</ListBucketResult>").encode()
        self.send_response(200); self.send_header("content-length", str(len(body))); self.end_headers()
        self.wfile.write(body)


class ClientTests(unittest.TestCase):
    def test_a_new_object_is_written_once_and_listed(self):
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), FakeS3)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        s3 = S3("bucket", "us-east-1", KEY_ID, SECRET, endpoint=f"http://127.0.0.1:{server.server_port}")
        self.assertTrue(s3.put_new("p/states/0000000001.ab.tree.age", b"one"))
        self.assertFalse(s3.put_new("p/states/0000000001.ab.tree.age", b"other"))
        self.assertTrue(s3.put_new("p/states/0000000002.cd.tree.age", b"two"))
        self.assertEqual(s3.list("p/states/"), ["p/states/0000000001.ab.tree.age", "p/states/0000000002.cd.tree.age"])
        self.assertEqual(FakeS3.objects["/bucket/p/states/0000000001.ab.tree.age"], b"one")


if __name__ == "__main__":
    unittest.main()
