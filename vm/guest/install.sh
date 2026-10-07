#!/bin/sh
# Install into a dedicated VM. The host provisions and mounts the data disk first.
set -eu
[ "$(id -u)" -eq 0 ] || { echo 'Run install.sh as root.' >&2; exit 1; }
[ "$(findmnt -n -o FSTYPE --target /var/lib/codex-studio)" = btrfs ] || {
    echo 'Mount the btrfs data disk at /var/lib/codex-studio first.' >&2
    exit 1
}
for tool in python3 rsync btrfs git unshare nsenter lsof; do
    command -v "$tool" >/dev/null || { echo "Install the required tool: $tool" >&2; exit 1; }
done
rsync --fsync --version >/dev/null 2>&1 || {
    echo 'Install rsync with --fsync support.' >&2
    exit 1
}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/../.." && pwd)
for script in codex_workspace_images.py codex_workspace_linux.py codex_process_supervisor.py codex_open_file_limit.py; do
    [ -f "$repo_dir/scripts/$script" ] || { echo "The runtime script is missing: $script" >&2; exit 1; }
done
[ "$repo_dir" = /opt/codex-studio ] || { echo 'Copy the runtime into /opt/codex-studio before installation.' >&2; exit 1; }
chown root:root "$repo_dir" "$repo_dir/vm" "$script_dir" "$repo_dir/scripts"
chmod 0755 "$repo_dir" "$repo_dir/vm" "$script_dir" "$repo_dir/scripts"
for file in "$script_dir"/*.py "$script_dir"/*.service "$repo_dir/scripts/codex_workspace_images.py" "$repo_dir/scripts/codex_workspace_linux.py" "$repo_dir/scripts/codex_process_supervisor.py" "$repo_dir/scripts/codex_open_file_limit.py"; do
    chown root:root "$file"
    chmod 0644 "$file"
done
id studio >/dev/null 2>&1 || useradd --create-home --home-dir /home/studio --shell /bin/bash studio
install -d -m 0700 -o studio -g studio /var/lib/codex-studio/guest /var/lib/codex-studio/projects /var/lib/codex-studio/workspaces
# Ubuntu restricts unprivileged namespaces through AppArmor. This dedicated VM
# needs them for the existing non-root workspace engine.
if [ -f /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]; then
    printf '%s\n' 'kernel.apparmor_restrict_unprivileged_userns=0' >/etc/sysctl.d/90-codex-studio-userns.conf
    sysctl -q -p /etc/sysctl.d/90-codex-studio-userns.conf
fi
sed "s|@REPO@|$repo_dir|g" "$script_dir/codex-studio-guest.service" >/etc/systemd/system/codex-studio-guest.service
install -m 0644 "$script_dir/codex-studio-grow.service" /etc/systemd/system/codex-studio-grow.service
systemctl daemon-reload
systemctl enable --now codex-studio-grow.service codex-studio-guest.service
