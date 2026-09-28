#!/usr/bin/env python3
"""Verify a release-local catalog patch against an explicit prior HTTP source.

Run with --patch /path/codex_*_update.py --baseline /path/codex_canvas.py.
Uses disposable state and never touches the running Studio server.
"""
import argparse
import importlib.util
from pathlib import Path
import sys
from types import ModuleType
import unittest
import urllib.error

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))


def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


parser = argparse.ArgumentParser()
parser.add_argument("--patch", type=Path, required=True)
parser.add_argument("--baseline", type=Path, required=True)
args, remaining = parser.parse_known_args()
patch = load(args.patch, "catalog_release_patch")
fixture = load(ROOT / "tests/skill-catalog-contract.py", "catalog_fixture")
baseline = ModuleType("catalog_prior_canvas")
baseline.__file__ = str(ROOT / "scripts/codex_canvas.py")
exec(compile(args.baseline.read_bytes(), baseline.__file__, "exec"), vars(baseline))
# The fixture creates the exact previous handler with the current test runtime.
fixture.make_server = baseline.make_server


class CatalogLiveUpdate(fixture.SkillCatalogContract):
    def setUp(self):
        super().setUp()
        self.original_handler_code = self.server.RequestHandlerClass.do_GET.__code__

    def get(self, agent="lead", headers=None):
        patch.apply(self.runtime)
        return super().get(agent, headers)

    def test_apply_and_lost_response_retry_preserve_identity(self):
        with self.assertRaises(urllib.error.HTTPError) as error:
            fixture.SkillCatalogContract.get(self)
        self.assertEqual(error.exception.code, 404)
        error.exception.close()
        handler = self.server.RequestHandlerClass.do_GET
        closure = handler.__closure__
        self.assertEqual(patch.apply(self.runtime), {"status": "applied"})
        self.assertEqual(self.runtime.calls, [], "Applying cannot start provider work")
        self.assertEqual(patch.apply(self.runtime), {"status": "already_applied"})
        self.assertIs(self.server.RequestHandlerClass.do_GET, handler)
        self.assertIs(handler.__closure__, closure)
        self.assertEqual(self.get(), {"skills": [], "errors": []})
        self.assertEqual(len(self.runtime.calls), 1)

    def test_unknown_running_revision_is_rejected_before_mutation(self):
        handler = self.server.RequestHandlerClass.do_GET
        changed = handler.__code__.replace(co_consts=handler.__code__.co_consts + ("unknown-revision",))
        handler.__code__ = changed
        with self.assertRaisesRegex(ValueError, "Unsupported running HTTP handler"):
            patch.apply(self.runtime)
        self.assertIs(handler.__code__, changed)
        self.assertEqual(self.runtime.calls, [])

    def test_exception_table_only_change_is_rejected_before_mutation(self):
        handler = self.server.RequestHandlerClass.do_GET
        self.assertTrue(handler.__code__.co_exceptiontable)
        changed = handler.__code__.replace(co_exceptiontable=b"")
        handler.__code__ = changed
        method = fixture.WorkspaceMixin.skill_catalog
        with self.assertRaisesRegex(ValueError, "Unsupported running HTTP handler"):
            patch.apply(self.runtime)
        self.assertIs(handler.__code__, changed)
        self.assertIs(fixture.WorkspaceMixin.skill_catalog, method)
        self.assertEqual(self.runtime.calls, [])


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0], *remaining])
