#!/bin/bash
# Run only in the dedicated Studio VM, after the data disk mounts.
set -Eeuo pipefail
test "$(id -u)" = 0
test "$(findmnt -n -o FSTYPE --target /var/lib/codex-studio)" = btrfs
source_dir=/opt/codex-studio/vm/layr
python3 - "$source_dir" <<'PY'
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
manifest = json.loads((root / 'manifest.json').read_text())
for name, expected in manifest['files'].items():
    path = root / name
    assert '..' not in path.parts and path.is_relative_to(root)
    assert hashlib.sha256(path.read_bytes()).hexdigest() == expected, name
assert hashlib.sha256(json.dumps(manifest['files'], sort_keys=True, separators=(',', ':')).encode()).hexdigest() == manifest['sourceSha256']
PY
identity=$(python3 -c 'import json; print(json.load(open("/opt/codex-studio/vm/layr/manifest.json"))["sourceSha256"])')
if [ "$(cat /var/lib/codex-studio/layr-build 2>/dev/null || true)" != "$identity" ] || [ ! -x /usr/local/bin/layr ]; then
    rust_version=1.90.0
    rust_dir=/opt/codex-studio/rust
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
    export CARGO_HOME=/var/lib/codex-studio/layr-build-cache/cargo
    export CARGO_TARGET_DIR=/var/lib/codex-studio/layr-build-cache/target
    timeout --kill-after=10 1200 cargo build --manifest-path "$source_dir/Cargo.toml" --release --locked --jobs 2
    install -m 0755 "$CARGO_TARGET_DIR/release/layr" /usr/local/bin/layr
    printf '%s\n' "$identity" > /var/lib/codex-studio/layr-build
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

