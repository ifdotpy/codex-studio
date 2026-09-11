#!/usr/bin/env python3
"""Native Chrome configuration, profile isolation, and explicit opt-outs."""

import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_browser import GUIDANCE, browser_config, configure_browser, ensure_skill, skill_root


class SkillServer:
    def __init__(self, fail=False):
        self.calls = []
        self.fail = fail

    def call(self, method, params, timeout):
        self.calls.append((method, params))
        if self.fail:
            raise RuntimeError("response timed out; outcome unknown")
        assert method == "skills/extraRoots/set"
        return {}


class BrowserContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name).resolve()
        self.shared, self.target = root / "shared", root / "target"
        self.shared.mkdir()
        self.skills = self.shared / "plugins/cache/openai-bundled/chrome/latest/skills"
        self.skill = self.skills / "control-chrome/SKILL.md"
        self.skill.parent.mkdir(parents=True)
        self.skill.write_text("---\nname: control-chrome\ndescription: Chrome control\n---\n")
        self.target.mkdir()
        self.market = root / "market"
        manifest = self.market / ".agents/plugins/marketplace.json"
        manifest.parent.mkdir(parents=True)
        manifest.write_text('{"name":"openai-bundled","plugins":[]}')
        self.binary, self.service = root / "node_repl", self.shared / "browser-service.mjs"
        self.binary.touch()
        self.service.touch()
        self.source = (
            '[marketplaces.openai-bundled]\nsource_type="local"\nsource=' + json.dumps(str(self.market)) + '\n'
            '[plugins."chrome@openai-bundled"]\nenabled=true\n'
            '[mcp_servers.node_repl]\ncommand=' + json.dumps(str(self.binary)) + '\nargs=[]\nstartup_timeout_sec=120\n'
            '[mcp_servers.node_repl.env]\nOPENAI_API_KEY="must-not-copy"\n'
            'CODEX_HOME=' + json.dumps(str(self.shared)) + '\n'
            'NODE_REPL_TRUSTED_SERVICES=' + json.dumps(json.dumps({"browser": str(self.service), "sky": "private-service"})) + '\n'
            'NODE_REPL_TRUSTED_CODE_PATHS=' + json.dumps(str(self.shared)) + '\n'
            '[mcp_servers.unrelated]\ncommand="must-not-copy"\n'
        )
        (self.shared / "config.toml").write_text(self.source)
        (self.target / "config.toml").write_text('model="gpt-6-astra"\n')
        (self.target / "auth.json").write_text('private-credentials')

    def config(self):
        return browser_config(self.target, self.shared)

    def test_native_configuration_without_profile_writes_or_credentials(self):
        before = {p: p.read_bytes() for home in (self.target, self.shared) for p in home.iterdir() if p.is_file()}
        result = self.config()
        self.assertEqual(set(result), {"mcp_servers.node_repl"})
        node = result["mcp_servers.node_repl"]
        self.assertEqual(node["env"]["CODEX_HOME"], str(self.target))
        self.assertEqual(json.loads(node["env"]["NODE_REPL_TRUSTED_SERVICES"]), {"browser": str(self.service)})
        self.assertNotIn("must-not-copy", json.dumps(result))
        self.assertNotIn("private-credentials", json.dumps(result))
        self.assertEqual(before, {p: p.read_bytes() for p in before})

    def test_account_local_node_settings_preserved_and_home_corrected(self):
        (self.target / "config.toml").write_text(
            '[mcp_servers.node_repl]\ncommand=' + json.dumps(str(self.binary)) + '\nstartup_timeout_sec=34\n'
            '[mcp_servers.node_repl.env]\nLOCAL_SETTING="keep"\nCODEX_HOME=' + json.dumps(str(self.shared)) + '\n'
        )
        node = self.config()["mcp_servers.node_repl"]
        self.assertEqual(node["startup_timeout_sec"], 34)
        self.assertEqual(node["env"]["LOCAL_SETTING"], "keep")
        self.assertEqual(node["env"]["CODEX_HOME"], str(self.target))

    def test_explicit_opt_outs_are_preserved(self):
        for content in ('[features]\nplugins=false', '[plugins."chrome@openai-bundled"]\nenabled=false', '[mcp_servers.node_repl]\nenabled=false'):
            with self.subTest(content=content):
                (self.target / "config.toml").write_text(content)
                self.assertEqual(self.config(), {})

    def test_custom_runtime_is_not_replaced(self):
        (self.target / "config.toml").write_text('[mcp_servers.node_repl]\ncommand="custom-runtime"')
        self.assertEqual(self.config(), {})

    def test_missing_browser_service_is_not_advertised(self):
        self.service.unlink()
        self.assertEqual(self.config(), {})

    def test_missing_skill_is_not_advertised(self):
        self.skill.unlink()
        self.assertEqual(self.config(), {})

    def test_invalid_config_error_does_not_expose_contents(self):
        (self.target / "config.toml").write_text('secret-credential=[bad')
        with self.assertRaisesRegex(ValueError, '^Cannot read the Codex profile configuration$'):
            self.config()

    def test_same_native_mechanism_for_leads_and_workers(self):
        server = SkillServer()
        runtime = SimpleNamespace(accounts=SimpleNamespace(home=lambda key: self.target, base_home=self.shared), connect=lambda key: server)
        for lead in (True, False):
            p = {"config": {"model_reasoning_effort": "max"}, "developerInstructions": "existing", "approvalPolicy": "never", "dynamicTools": [{"name": "existing"}]}
            configure_browser(runtime, {"accountKey": "second", "isLead": lead}, p)
            self.assertEqual(p["config"]["model_reasoning_effort"], "max")
            self.assertEqual(p["approvalPolicy"], "never")
            self.assertEqual(p["dynamicTools"], [{"name": "existing"}])
            self.assertTrue(p["developerInstructions"].endswith(GUIDANCE))
            self.assertNotIn("SKILL.md", p["developerInstructions"])
        self.assertEqual(len(server.calls), 1)

    def test_registers_skill_once_per_connection(self):
        first, second = SkillServer(), SkillServer()
        for _ in range(3):
            ensure_skill(first, self.skills)
        ensure_skill(second, self.skills)
        self.assertEqual(first.calls, [("skills/extraRoots/set", {"extraRoots": [str(self.skills)]})])
        self.assertEqual(first.calls, second.calls)

    def test_unknown_registration_can_safely_repeat(self):
        server = SkillServer(fail=True)
        with self.assertRaisesRegex(RuntimeError, "outcome unknown"):
            ensure_skill(server, self.skills)
        server.fail = False
        ensure_skill(server, self.skills)
        self.assertEqual(server.calls[0], server.calls[1])

    def test_updated_installed_version_replaces_root(self):
        server = SkillServer()
        ensure_skill(server, self.skills)
        ensure_skill(server, self.skills / "new-version")
        self.assertEqual(len(server.calls), 2)

    def test_disable_removes_host_registration_after_unknown_reply(self):
        server = SkillServer(fail=True)
        with self.assertRaises(RuntimeError):
            ensure_skill(server, self.skills)
        server.fail = False
        runtime = SimpleNamespace(accounts=SimpleNamespace(home=lambda key: self.target, base_home=self.shared), servers={"second": server})
        (self.target / "config.toml").write_text('[plugins."chrome@openai-bundled"]\nenabled=false')
        params = {"config": {}, "developerInstructions": "existing"}
        configure_browser(runtime, {"accountKey": "second"}, params)
        self.assertEqual(server.calls[-1], ("skills/extraRoots/set", {"extraRoots": []}))
        self.assertEqual(params, {"config": {}, "developerInstructions": "existing"})

    def test_native_config_keys_are_valid_toml_paths(self):
        import tomllib
        for key, value in self.config().items():
            # The native app-server interprets override keys as TOML key paths.
            tomllib.loads(key + '=true')


if __name__ == "__main__":
    unittest.main()
