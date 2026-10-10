"""Canonical paths for the relocated Codex Studio workspace layout."""
from pathlib import Path


SERVER_SOURCE_ROOT = Path(__file__).resolve().parent

if SERVER_SOURCE_ROOT.name == "scripts":
    # Packaged desktop resources keep the old workspace/scripts tree shape.
    REPOSITORY_ROOT = SERVER_SOURCE_ROOT.parent
    SERVER_APP_ROOT = REPOSITORY_ROOT
    SERVER_TESTS_ROOT = SERVER_SOURCE_ROOT.parent / "tests"
    RUNTIME_ROOT = REPOSITORY_ROOT
    WORKSPACES_ROOT = REPOSITORY_ROOT
    # The package copies the Claude bridge source for the VM guest payload here
    # (workspaces/client/apps/desktop/package.mjs).
    PROVIDERS_ROOT = REPOSITORY_ROOT / "workspaces" / "providers"
    CLIENT_ROOT = REPOSITORY_ROOT
    TOOLING_ROOT = REPOSITORY_ROOT
    CLAUDE_BRIDGE_ROOT = SERVER_SOURCE_ROOT / "claude_bridge"
    WEB_ROOT = REPOSITORY_ROOT / "web"
    DESKTOP_ROOT = REPOSITORY_ROOT / "desktop"
    VM_GUEST_ROOT = REPOSITORY_ROOT / "vm" / "guest"
    PROMPTS_ROOT = REPOSITORY_ROOT / "prompts"
else:
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
