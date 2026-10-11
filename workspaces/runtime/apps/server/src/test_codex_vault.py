"""codex_vault.py configure: the configuration on stdin reaches vault.configure with its optional
signing key, and nothing else of it."""
from __future__ import annotations

import contextlib
import io
import json
from pathlib import Path
import tempfile
from typing import Any
import unittest
from unittest.mock import patch

import codex_vault

CONFIG = {"bucket": "bucket-one", "region": "eu-central-1", "prefix": "app", "recipient": "age1" + "q" * 58,
          "accessKeyId": "AKIAIOSFODNN7EXAMPLE", "secretAccessKey": "s" * 40}


class FakeClient:
    calls: list[tuple[str, dict[str, Any]]] = []

    def __init__(self) -> None:
        self.state_dir = Path(tempfile.gettempdir())

    def call(self, method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
        self.calls.append((method, params))
        return {"configured": True}


class ConfigureTests(unittest.TestCase):
    def configure(self, given: dict[str, Any]) -> dict[str, Any]:
        FakeClient.calls = []
        with patch("codex_linux_vm.Client", FakeClient), patch("sys.stdin", io.StringIO(json.dumps(given))), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(codex_vault.main(["configure", "/tmp/project"]), 0)
        [(method, params)] = FakeClient.calls
        self.assertEqual(method, "vault.configure")
        return params

    def test_the_signing_key_is_passed_when_present_and_not_required(self) -> None:
        params = self.configure({**CONFIG, "signingKey": "a2V5", "other": "dropped"})
        self.assertEqual(params["signingKey"], "a2V5")
        self.assertNotIn("other", params)
        params = self.configure(CONFIG)
        self.assertNotIn("signingKey", params)
        self.assertEqual({key: params[key] for key in CONFIG}, CONFIG)


if __name__ == "__main__":
    unittest.main()
