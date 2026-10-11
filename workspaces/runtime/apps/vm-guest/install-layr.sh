#!/bin/bash
# Run only in the dedicated Studio VM, after the data disk mounts.
set -Eeuo pipefail
test "$(id -u)" = 0
test "$(findmnt -n -o FSTYPE --target /var/lib/codex-studio)" = btrfs
rust_version=1.90.0
rust_dir=/opt/codex-studio/rust
export CARGO_HOME=/var/lib/codex-studio/layr-build-cache/cargo

# The identity of a vendored source (update-vm-layr.py), after each of its files matches the manifest.
source_identity() {
    python3 - "$1" <<'PY'
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
manifest = json.loads((root / 'manifest.json').read_text())
for name, expected in manifest['files'].items():
    path = root / name
    assert '..' not in path.parts and path.is_relative_to(root)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == expected, name
assert hashlib.sha256(json.dumps(manifest['files'], sort_keys=True, separators=(',', ':')).encode()).hexdigest() == manifest['sourceSha256']
print(manifest['sourceSha256'])
PY
}

# The Rust toolchain of both builds. Cargo takes the crates of each lock file from crates.io.
use_rust() {
    if [ ! -x "$rust_dir/bin/cargo" ]; then
        archive=rust-$rust_version-aarch64-unknown-linux-gnu.tar.xz
        url=https://static.rust-lang.org/dist/$archive
        curl --fail --location --silent --show-error --connect-timeout 20 --max-time 600 -o /tmp/$archive "$url"
        curl --fail --location --silent --show-error --connect-timeout 20 --max-time 60 -o /tmp/$archive.sha256 "$url.sha256"
        (cd /tmp && sha256sum -c "$archive.sha256")
        tar -xJf /tmp/$archive -C /tmp
        /tmp/rust-$rust_version-aarch64-unknown-linux-gnu/install.sh --prefix="$rust_dir" --components=rustc,cargo,rust-std-aarch64-unknown-linux-gnu --disable-ldconfig
        rm -rf /tmp/$archive /tmp/$archive.sha256 /tmp/rust-$rust_version-aarch64-unknown-linux-gnu
    fi
    export PATH="$rust_dir/bin:$PATH"
}

source_dir=/opt/codex-studio/vm/layr
identity=$(source_identity "$source_dir")
if [ "$(cat /var/lib/codex-studio/layr-build 2>/dev/null || true)" != "$identity" ] || [ ! -x /usr/local/bin/layr ]; then
    use_rust
    export CARGO_TARGET_DIR=/var/lib/codex-studio/layr-build-cache/target
    timeout --kill-after=10 1200 cargo build --manifest-path "$source_dir/Cargo.toml" --release --locked --jobs 2
    install -m 0755 "$CARGO_TARGET_DIR/release/layr" /usr/local/bin/layr
    printf '%s\n' "$identity" > /var/lib/codex-studio/layr-build
    rebuilt=1
fi
# The vault writer (docs/vault.md): the root broker runs `layr-desk push`; it has no daemon.
desk_dir=/opt/codex-studio/vm/layr-desk
desk_identity=$(source_identity "$desk_dir")
if [ "$(cat /var/lib/codex-studio/layr-desk-build 2>/dev/null || true)" != "$desk_identity" ] || [ ! -x /usr/local/bin/layr-desk ]; then
    use_rust
    export CARGO_TARGET_DIR=/var/lib/codex-studio/layr-build-cache/layr-desk-target
    timeout --kill-after=10 1200 cargo build --manifest-path "$desk_dir/Cargo.toml" --release --locked --jobs 2 -p layr-desk
    install -m 0755 "$CARGO_TARGET_DIR/release/layr-desk" /usr/local/bin/layr-desk
    printf '%s\n' "$desk_identity" > /var/lib/codex-studio/layr-desk-build
fi
install -d -m 0711 /var/lib/codex-studio/layr
install -d -m 0700 /var/lib/codex-studio/layr-admin
for name in layr layr-admin share share-bridge; do
    install -m 0644 /opt/codex-studio/vm/guest/codex-studio-$name.service /etc/systemd/system/codex-studio-$name.service
done
# Only our unit serves SMB, on the guest loopback; the vsock bridge carries it to the Mac.
systemctl disable --now smbd.service nmbd.service 2>/dev/null || true
systemctl daemon-reload
systemctl enable --now codex-studio-layr.service codex-studio-layr-admin.service codex-studio-share-bridge.service
# The daemon starts at boot, before provisioning rebuilds layr: a new binary needs a restart
# (on 2026-10-11 the old daemon kept reading .regenerable after the .layrattributes release).
if [ "${rebuilt:-0}" = 1 ]; then systemctl restart codex-studio-layr.service; fi
