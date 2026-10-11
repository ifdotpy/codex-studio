#!/usr/bin/env python3
"""Converge the vault gateway with its artifacts bucket (docs/vault.md, "Gateway").

Runs as root at first boot and then every 5 minutes from its own timer. Each step is idempotent:
join the tailnet, install a new release, write the configuration, point the host at the tailnet
address, get the certificate, and keep the gateway service running.
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

os.environ.setdefault("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE", "IPv6")
os.environ.setdefault("AWS_USE_DUALSTACK_ENDPOINT", "true")
import boto3  # noqa: E402

SOURCE = Path("/etc/vault-gateway/source.json")
CONFIG = Path("/etc/vault-gateway/config.json")
APP = Path("/opt/vault-gateway")
UNITS = Path("/etc/systemd/system")
HOOK = Path("/etc/letsencrypt/renewal-hooks/deploy/vault-gateway")
ENV = (
    "Environment=AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE=IPv6\n"
    "Environment=AWS_USE_DUALSTACK_ENDPOINT=true\n"
)

SYNC_SERVICE = f"""[Unit]
Description=Studio vault gateway sync
After=network-online.target tailscaled.service
Wants=network-online.target

[Service]
Type=oneshot
{ENV}ExecStart=/usr/bin/python3 {APP}/current/sync.py
"""
SYNC_TIMER = """[Unit]
Description=Studio vault gateway sync every 5 minutes

[Timer]
OnBootSec=1min
OnUnitActiveSec=5min

[Install]
WantedBy=timers.target
"""


def gateway_service(host: str) -> str:
    live = f"/etc/letsencrypt/live/{host}"
    return f"""[Unit]
Description=Studio vault gateway
After=network-online.target tailscaled.service
Wants=network-online.target

[Service]
ExecStart=/usr/bin/node {APP}/current/gateway.mjs
{ENV}LoadCredential=tls.crt:{live}/fullchain.pem
LoadCredential=tls.key:{live}/privkey.pem
DynamicUser=yes
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=yes
LockPersonality=yes
SystemCallArchitectures=native
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
"""


def log(*parts: object) -> None:
    print(*parts, flush=True)


def run(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, check=check, text=True, capture_output=True)


def write_if_changed(path: Path, text: str, mode: int = 0o644) -> bool:
    if path.exists() and path.read_text() == text:
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", dir=path.parent, delete=False) as tmp:
        tmp.write(text)
    os.chmod(tmp.name, mode)
    os.replace(tmp.name, path)
    return True


class Sync:
    def __init__(self) -> None:
        self.source = json.loads(SOURCE.read_text())
        self.s3 = boto3.client("s3", region_name=self.source["region"])
        self.bucket = self.source["bucket"]
        self.restart = False

    def get(self, key: str) -> bytes:
        return self.s3.get_object(Bucket=self.bucket, Key=key)["Body"].read()

    def tailnet(self) -> str | None:
        """Join with the stored auth key once; return the tailnet IPv4 address."""
        state = json.loads(run("tailscale", "status", "--json", check=False).stdout or "{}")
        if state.get("BackendState") != "Running":
            ssm = boto3.client("ssm", region_name=self.source["region"])
            key = ssm.get_parameter(Name=self.source["authKeyParameter"], WithDecryption=True)
            with tempfile.NamedTemporaryFile("w", dir="/run", delete=False) as tmp:
                tmp.write(key["Parameter"]["Value"])
            os.chmod(tmp.name, 0o600)
            try:
                run(
                    "tailscale", "up", f"--auth-key=file:{tmp.name}", "--hostname=vault-gateway",
                    "--advertise-tags=tag:vault", "--ssh",
                )
            finally:
                os.unlink(tmp.name)
            log("joined the tailnet")
        address = run("tailscale", "ip", "-4", check=False).stdout.strip()
        return address or None

    def release(self) -> None:
        """Install the release that gateway/release.json names, and keep the previous one."""
        release = json.loads(self.get("gateway/release.json"))
        current = APP / "current"
        if (current / ".sha256").exists() and (current / ".sha256").read_text() == release["sha256"]:
            return
        data = self.get(release["key"])
        if hashlib.sha256(data).hexdigest() != release["sha256"]:
            raise RuntimeError(f"{release['key']} does not match its SHA-256")
        target = APP / "releases" / release["sha256"]
        shutil.rmtree(target, ignore_errors=True)
        target.mkdir(parents=True)
        with tarfile.open(fileobj=io.BytesIO(data)) as tar:
            tar.extractall(target, filter="data")
        (target / ".sha256").write_text(release["sha256"])
        link = APP / "current.new"
        link.unlink(missing_ok=True)
        link.symlink_to(target)
        os.replace(link, current)
        # Keep this release and the one before it.
        releases = sorted((APP / "releases").iterdir(), key=lambda p: p.stat().st_mtime)
        for old in releases[:-2]:
            if old != target:
                shutil.rmtree(old, ignore_errors=True)
        self.restart = True
        log("installed release", release["sha256"][:12])

    def configure(self, listen: str | None) -> dict:
        gateway = json.loads(self.get("gateway/gateway.json"))
        projects = {}
        pages = self.s3.get_paginator("list_objects_v2")
        for page in pages.paginate(Bucket=self.bucket, Prefix="gateway/projects/"):
            for item in page.get("Contents", []):
                name = item["Key"].removeprefix("gateway/projects/").removesuffix(".json")
                projects[name] = json.loads(self.get(item["Key"]))
        config = {"host": gateway["host"], "listen": listen, "projects": projects}
        old = json.loads(CONFIG.read_text()) if CONFIG.exists() else {}
        if write_if_changed(CONFIG, json.dumps(config, indent=2, sort_keys=True) + "\n"):
            log("wrote the configuration:", ", ".join(sorted(projects)) or "no vaults")
            # The server reads the projects on each request, but the address only at start.
            if old.get("listen") != listen:
                self.restart = True
        return gateway

    def dns(self, gateway: dict, listen: str) -> None:
        route53 = boto3.client("route53", region_name=self.source["region"])
        host = gateway["host"].rstrip(".") + "."
        found = route53.list_resource_record_sets(
            HostedZoneId=gateway["zone"], StartRecordName=host, StartRecordType="A", MaxItems="1"
        )["ResourceRecordSets"]
        if found and found[0]["Name"] == host and found[0]["Type"] == "A":
            if [r["Value"] for r in found[0]["ResourceRecords"]] == [listen]:
                return
        route53.change_resource_record_sets(
            HostedZoneId=gateway["zone"],
            ChangeBatch={
                "Comment": "vault gateway tailnet address",
                "Changes": [
                    {
                        "Action": "UPSERT",
                        "ResourceRecordSet": {
                            "Name": host,
                            "Type": "A",
                            "TTL": 300,
                            "ResourceRecords": [{"Value": listen}],
                        },
                    }
                ],
            },
        )
        log("pointed", host, "at", listen)

    def certificate(self, host: str) -> bool:
        if not Path(f"/etc/letsencrypt/live/{host}/fullchain.pem").exists():
            run(
                "certbot", "certonly", "--dns-route53", "--non-interactive", "--agree-tos",
                "--register-unsafely-without-email", "--key-type", "ecdsa", "-d", host,
            )
            log("got the certificate for", host)
        write_if_changed(HOOK, "#!/bin/sh\nsystemctl restart vault-gateway.service\n", 0o755)
        return True

    def services(self, host: str | None) -> None:
        changed = write_if_changed(UNITS / "vault-gateway-sync.service", SYNC_SERVICE)
        changed |= write_if_changed(UNITS / "vault-gateway-sync.timer", SYNC_TIMER)
        if host:
            if write_if_changed(UNITS / "vault-gateway.service", gateway_service(host)):
                changed = self.restart = True
        if changed:
            run("systemctl", "daemon-reload")
        run("systemctl", "enable", "--now", "vault-gateway-sync.timer")
        if not host:
            return
        active = run("systemctl", "is-active", "vault-gateway.service", check=False).stdout.strip()
        if self.restart or active != "active":
            run("systemctl", "enable", "vault-gateway.service")
            run("systemctl", "restart", "vault-gateway.service")
            log("restarted the gateway")

    def run(self) -> int:
        """Each step that fails is logged and runs again in the next round."""
        failed = 0

        def attempt(name, step):
            nonlocal failed
            try:
                return step()
            except Exception as error:
                failed += 1
                log(f"{name}: {type(error).__name__}: {error}")

        listen = attempt("tailnet", self.tailnet)
        attempt("release", self.release)
        gateway = attempt("configure", lambda: self.configure(listen))
        host = None
        if gateway and listen:
            attempt("dns", lambda: self.dns(gateway, listen))
            if attempt("certificate", lambda: self.certificate(gateway["host"])):
                host = gateway["host"]
        attempt("services", lambda: self.services(host))
        return 1 if failed else 0

if __name__ == "__main__":
    sys.exit(Sync().run())
