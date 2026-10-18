"""Check schema identity parity and disposable fixture cache isolation."""

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from test_isolation import (
    generated_api_schema_hash,
    isolate_supervisor_environment,
    seed_api_schema_cache_for_fixture,
)

isolate_supervisor_environment()


class ApiSchemaFixtureCacheTests(unittest.TestCase):
    def test_generated_schema_identity_matches_runtime_computation(self):
        from studio_api.context import compute_api_schema_hash_in_subprocess

        self.assertEqual(
            compute_api_schema_hash_in_subprocess(),
            generated_api_schema_hash(),
        )

    def test_schema_fixture_cache_uses_generated_identity_in_disposable_cache(self):
        from studio_api.context import (
            API_SCHEMA_CACHE_DIRECTORY,
            API_SCHEMA_CACHE_FILE,
            api_schema_cache_key,
            read_cached_or_compute_api_schema_hash,
        )

        with tempfile.TemporaryDirectory(prefix="studio-test-schema-cache-") as cache_home:
            cache_root = Path(cache_home)
            generated_hash = generated_api_schema_hash()
            previous_cache_home = os.environ.get("XDG_CACHE_HOME")
            self.assertEqual(seed_api_schema_cache_for_fixture(cache_root), generated_hash)
            self.assertEqual(os.environ.get("XDG_CACHE_HOME"), previous_cache_home)

            cache_path = cache_root / API_SCHEMA_CACHE_DIRECTORY / API_SCHEMA_CACHE_FILE
            entry = json.loads(cache_path.read_text(encoding="utf-8"))
            self.assertEqual(
                entry,
                {"key": api_schema_cache_key(), "hash": generated_hash},
            )
            self.assertTrue(cache_path.is_relative_to(cache_root))

            with patch.dict(os.environ, {"XDG_CACHE_HOME": str(cache_root)}):
                self.assertEqual(
                    read_cached_or_compute_api_schema_hash(
                        api_schema_cache_key(),
                        lambda: self.fail("fixture cache should avoid schema reconstruction"),
                    ),
                    generated_hash,
                )


if __name__ == "__main__":
    unittest.main()
