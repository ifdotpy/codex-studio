#!/bin/sh
# Run during guest provisioning, after installation of the pinned Codex binary.
set -eu
[ "$(id -u)" -eq 0 ] || { echo 'Run install-codex-sandbox.sh as root.' >&2; exit 1; }
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# Older release extraction preserved archive UIDs. A layr user can receive that
# UID, so repair ownership before granting the executable a userns profile.
[ ! -L /opt/codex-studio/codex ] || { echo 'The Codex installation must not be a symlink.' >&2; exit 1; }
if [ -d /opt/codex-studio/codex ]; then
    chown -R --no-dereference root:root /opt/codex-studio/codex
    chmod -R go-w /opt/codex-studio/codex
fi
if [ -f /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]; then
    command -v apparmor_parser >/dev/null || { echo 'Install apparmor_parser for the Codex sandbox.' >&2; exit 1; }
    install -m 0644 -o root -g root "$script_dir/studio-codex.apparmor" /etc/apparmor.d/studio-codex
    apparmor_parser -r /etc/apparmor.d/studio-codex
fi
