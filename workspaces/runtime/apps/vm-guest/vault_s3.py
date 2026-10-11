"""The S3 calls of the vault writer, signed with AWS Signature Version 4 (no SDK in the guest).

Only what a write-only key may do: put a new object (If-None-Match: *) and list names.
"""
from __future__ import annotations

import datetime
import hashlib
import hmac
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET

EMPTY = hashlib.sha256(b"").hexdigest()


def quote(text, safe="/"):
    return urllib.parse.quote(text, safe=safe + "-_.~")


def query_string(query):
    """The query as S3 signs it; the URL uses the same text."""
    return "&".join(quote(k, "") + "=" + quote(v, "") for k, v in sorted(query.items()))


def sign(method, host, path, query, headers, payload_hash, *, key_id, secret, region, now, service="s3"):
    """The Authorization header for one request. `headers` holds the headers to sign, besides
    host, x-amz-date and x-amz-content-sha256, which this adds; returns all signed headers."""
    stamp = now.strftime("%Y%m%dT%H%M%SZ")
    day = stamp[:8]
    signed = {k.lower(): str(v).strip() for k, v in headers.items()}
    signed.update({"host": host, "x-amz-date": stamp, "x-amz-content-sha256": payload_hash})
    names = sorted(signed)
    canonical_query = query_string(query)
    canonical = "\n".join([method, quote(path), canonical_query,
                           "".join(f"{n}:{signed[n]}\n" for n in names), ";".join(names), payload_hash])
    scope = f"{day}/{region}/{service}/aws4_request"
    to_sign = "\n".join(["AWS4-HMAC-SHA256", stamp, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    key = ("AWS4" + secret).encode()
    for part in (day, region, service, "aws4_request"):
        key = hmac.new(key, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    signed["authorization"] = (f"AWS4-HMAC-SHA256 Credential={key_id}/{scope}, "
                               f"SignedHeaders={';'.join(names)}, Signature={signature}")
    return signed


class S3:
    def __init__(self, bucket, region, key_id, secret, *, endpoint=None, timeout=120):
        self.bucket, self.region, self.key_id, self.secret = bucket, region, key_id, secret
        # Path-style for a test endpoint; virtual-hosted style for AWS.
        self.endpoint = endpoint
        self.host = urllib.parse.urlsplit(endpoint).netloc if endpoint else f"{bucket}.s3.{region}.amazonaws.com"
        self.timeout = timeout

    def _request(self, method, key, query=None, body=b"", headers=None):
        path = (f"/{self.bucket}/" if self.endpoint else "/") + key
        query = query or {}
        payload = hashlib.sha256(body).hexdigest() if body else EMPTY
        signed = sign(method, self.host, path, query, headers or {}, payload, key_id=self.key_id,
                      secret=self.secret, region=self.region, now=datetime.datetime.now(datetime.timezone.utc))
        base = self.endpoint or f"https://{self.host}"
        url = base + quote(path) + ("?" + query_string(query) if query else "")
        request = urllib.request.Request(url, data=body if method == "PUT" else None, method=method,
                                         headers={k: v for k, v in signed.items() if k != "host"})
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()

    def put_new(self, key, body):
        """Put an object that must not exist yet. True if written, False if it already exists."""
        status, data = self._request("PUT", key, body=body, headers={"if-none-match": "*"})
        if status == 412:
            return False
        if status != 200:
            raise OSError(f"S3 put {key}: HTTP {status}: {data[:300]!r}")
        return True

    def list(self, prefix):
        """Every key under `prefix`."""
        keys, token = [], None
        while True:
            query = {"list-type": "2", "prefix": prefix}
            if token:
                query["continuation-token"] = token
            status, data = self._request("GET", "", query=query)
            if status != 200:
                raise OSError(f"S3 list {prefix}: HTTP {status}: {data[:300]!r}")
            root = ET.fromstring(data)
            ns = {"s": root.tag.split("}")[0].strip("{")} if root.tag.startswith("{") else {}
            find = (lambda e, n: e.find(f"s:{n}", ns)) if ns else (lambda e, n: e.find(n))
            for item in root.findall("s:Contents" if ns else "Contents", ns):
                keys.append(find(item, "Key").text)
            nxt = find(root, "NextContinuationToken")
            if nxt is None or not nxt.text:
                return keys
            token = nxt.text
