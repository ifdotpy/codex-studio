"""Expose the installed OpenAI Chrome skill through native host registration.

Codex owns skill injection, MCP tools, and browser permissions.
Only local browser runtime configuration is shared across account homes.
"""

from copy import deepcopy
import json
import os
import sys
from pathlib import Path
import tomllib
import threading
from weakref import WeakKeyDictionary


PLUGIN = "chrome@openai-bundled"
GUIDANCE = (
    "For browser interaction, use the installed control-chrome skill and its "
    "browser-client through node_repl by default. Honor an explicitly requested different "
    "browser or tool. Read the native skill before use; it owns setup, permissions, "
    "tab lifecycle, and recovery. Report connection failures instead of silently changing "
    "the browser-control method. If native Chrome discovery reports no browser, finish the "
    "turn after reporting the error; Studio can refresh the connection at an idle boundary. "
    "Do not repeat any browser action with an unknown outcome. Dedicated connectors and web search remain appropriate "
    "for tasks that do not need browser interaction."
)

# Do not inherit account credentials, arbitrary MCP servers, or computer-use settings.
ENV_KEYS = {
    "NODE_REPL_NATIVE_PIPE_CONNECT_TIMEOUT_MS", "NODE_REPL_NODE_MODULE_DIRS",
    "NODE_REPL_NODE_PATH", "NODE_REPL_TRUSTED_CODE_PATHS",
    "NODE_REPL_INSTRUCTIONS_USE_CASE_CHROME", "BROWSER_USE_CODEX_APP_BUILD_FLAVOR",
    "BROWSER_USE_CODEX_APP_VERSION", "BROWSER_USE_TINYSKY_ENABLED", "CODEX_CLI_PATH",
}

_SKILL_LOCK = threading.Lock()
_SKILL_ROOTS = WeakKeyDictionary()


def _plugin_version(plugin_dir):
    try:
        value = json.loads((Path(plugin_dir) / ".codex-plugin/plugin.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    version = value.get("version") if isinstance(value, dict) else None
    return version if isinstance(version, str) and version else None


def skill_root(shared_home):
    """Use the desktop's installed version pointer, not a guessed plugin version.

    Codex 26.917 installs the plugin cache without skills and reads them from the
    local bundled marketplace. Accept that copy only for the same plugin version,
    so the skill and its browser-client come from one release.
    """
    installed = Path(shared_home) / "plugins/cache/openai-bundled/chrome/latest"
    root = installed / "skills"
    if (root / "control-chrome/SKILL.md").is_file():
        return root.resolve()
    try:
        market = read_config(shared_home).get("marketplaces", {}).get("openai-bundled", {})
    except ValueError:
        return None
    if market.get("source_type") != "local" or not isinstance(market.get("source"), str):
        return None
    plugin = Path(market["source"]) / "plugins/chrome"
    root = plugin / "skills"
    version = _plugin_version(plugin)
    if (not (root / "control-chrome/SKILL.md").is_file()
            or not (plugin / "scripts/browser-client.mjs").is_file()
            or version is None or version != _plugin_version(installed)):
        return None
    return root.resolve()


def ensure_skill(server, root):
    """Register host skill roots through Codex. Studio owns these connections."""
    # No other Studio component sets extra roots. Keep this registration centralized.
    # Setting the same roots is idempotent, including after an unknown response.
    with _SKILL_LOCK:
        state = _SKILL_ROOTS.setdefault(server, {"lock": threading.Lock(), "root": None, "dirty": False})
    with state["lock"]:
        if not state["dirty"] and state["root"] == root:
            return
        state["dirty"] = True
        server.call("skills/extraRoots/set", {"extraRoots": [str(root)] if root else []}, timeout=20)
        state.update(root=root, dirty=False)


def read_config(home):
    path = Path(home) / "config.toml"
    if not path.exists():
        return {}
    # Do not include config contents (which can contain credentials) in errors.
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        raise ValueError("Cannot read the Codex profile configuration") from None


def browser_config(home, shared_home):
    """Return thread/start and thread/resume overrides without editing either home."""
    return browser_status(home, shared_home)[0]


def browser_status(home, shared_home):
    """Return the overrides and, when they are empty, the reason."""
    home, shared_home = Path(home).resolve(), Path(shared_home).resolve()
    target, shared = read_config(home), read_config(shared_home)
    if target.get("features", {}).get("plugins") is False:
        return {}, "Plugins are disabled in this account's Codex configuration"
    if target.get("plugins", {}).get(PLUGIN, {}).get("enabled") is False:
        return {}, "The Chrome plugin is disabled in this account's Codex configuration"
    source_node = shared.get("mcp_servers", {}).get("node_repl", {})
    target_node = target.get("mcp_servers", {}).get("node_repl")
    if target_node is not None and target_node.get("enabled") is False:
        return {}, "node_repl is disabled in this account's Codex configuration"
    if skill_root(shared_home) is None:
        return {}, "The control-chrome skill for the installed Chrome plugin version was not found"
    if not source_node.get("command") or not Path(source_node["command"]).is_file():
        return {}, "The node_repl runtime from the Codex configuration is missing"
    # A profile's custom runtime remains its own. Do not replace it with another binary.
    if target_node and target_node.get("command") != source_node["command"]:
        return {}, "This account uses its own node_repl runtime"
    try:
        services = json.loads(source_node.get("env", {}).get("NODE_REPL_TRUSTED_SERVICES", "{}"))
    except (ValueError, TypeError):
        return {}, "NODE_REPL_TRUSTED_SERVICES is not valid JSON"
    browser_service = services.get("browser") if isinstance(services, dict) else None
    if not isinstance(browser_service, str) or not Path(browser_service).is_file():
        return {}, "The browser service file is missing"
    if target_node is not None:
        node = deepcopy(target_node)
    else:
        node = {k: deepcopy(source_node[k]) for k in ("command", "args", "startup_timeout_sec") if k in source_node}
        node["env"] = {k: v for k, v in source_node.get("env", {}).items() if k in ENV_KEYS}
    env = node.setdefault("env", {})
    # The native browser service follows this account's rollout for turn completion.
    env["CODEX_HOME"] = str(home)
    trusted = env.get("NODE_REPL_TRUSTED_CODE_PATHS", "").split(os.pathsep)
    env["NODE_REPL_TRUSTED_CODE_PATHS"] = os.pathsep.join(dict.fromkeys(
        p for p in [*trusted, str(shared_home), str(home)] if p
    ))
    if "NODE_REPL_TRUSTED_SERVICES" not in env:
        env["NODE_REPL_TRUSTED_SERVICES"] = json.dumps({"browser": browser_service})
    env.setdefault("BROWSER_USE_AVAILABLE_BACKENDS", "chrome")
    env.setdefault("NODE_REPL_INSTRUCTIONS_USE_CASE_CHROME", GUIDANCE)
    return {
        "mcp_servers.node_repl": node,
    }, None


_LAST_REASON = {}


def configure_browser(runtime, actor, params):
    key = actor.get("accountKey", "default")
    config, reason = browser_status(runtime.accounts.home(key), runtime.accounts.base_home)
    if _LAST_REASON.get(key, "") != (reason or ""):
        # Report each change once. A disabled browser must not be silent.
        _LAST_REASON[key] = reason or ""
        print(f"Studio browser for account {key}: " + (reason or "enabled"), file=sys.stderr, flush=True)
    params["config"].update(config)
    if config:
        ensure_skill(runtime.connect(actor.get("accountKey", "default")), skill_root(runtime.accounts.base_home))
        params["developerInstructions"] += "\n" + GUIDANCE
    else:
        server = getattr(runtime, "servers", {}).get(actor.get("accountKey", "default"))
        if server is not None:
            ensure_skill(server, None)
    return params


def diagnostics(runtime):
    """Browser state of the default account for /api/desktop."""
    accounts = getattr(runtime, "accounts", None)
    if accounts is None:
        return None
    try:
        config, reason = browser_status(accounts.home("default"), accounts.base_home)
    except (OSError, ValueError) as error:
        return {"enabled": False, "reason": str(error)}
    root = skill_root(accounts.base_home)
    return {"enabled": bool(config), "reason": reason, "skillRoot": str(root) if root else None}
