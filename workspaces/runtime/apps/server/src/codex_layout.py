"""Canonical paths for the relocated Codex Studio workspace layout."""
from pathlib import Path


SERVER_SOURCE_ROOT = Path(__file__).resolve().parent
SERVER_APP_ROOT = SERVER_SOURCE_ROOT.parent
SERVER_TESTS_ROOT = SERVER_APP_ROOT / "tests"

RUNTIME_ROOT = SERVER_APP_ROOT.parent.parent
WORKSPACES_ROOT = RUNTIME_ROOT.parent
REPOSITORY_ROOT = WORKSPACES_ROOT.parent

PROVIDERS_ROOT = WORKSPACES_ROOT / "providers"
CLIENT_ROOT = WORKSPACES_ROOT / "client"
TOOLING_ROOT = WORKSPACES_ROOT / "tooling"

CLAUDE_BRIDGE_ROOT = PROVIDERS_ROOT / "apps" / "claude-bridge"
WEB_ROOT = CLIENT_ROOT / "apps" / "web"
DESKTOP_ROOT = CLIENT_ROOT / "apps" / "desktop"
VM_GUEST_ROOT = RUNTIME_ROOT / "apps" / "vm-guest"
PROMPTS_ROOT = SERVER_APP_ROOT / "prompts"
