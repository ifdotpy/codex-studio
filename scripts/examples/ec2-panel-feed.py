#!/usr/bin/env python3
"""Read configured EC2 instances and emit panel state as newline-delimited JSON."""

import argparse
from collections import defaultdict
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import threading


STOP = threading.Event()
INSTANCE_ID = re.compile(r"i-(?:[a-f0-9]{8}|[a-f0-9]{17})\Z")


def load_config(path):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    resources = data.get("resources") if isinstance(data, dict) else None
    if not isinstance(resources, list) or not 1 <= len(resources) <= 12:
        raise ValueError("config.resources must contain 1 to 12 explicit instances")
    seen = set()
    for resource in resources:
        if not isinstance(resource, dict):
            raise ValueError("each resource must be an object")
        for key, limit in (("instanceId", 21), ("profile", 128), ("region", 40), ("label", 24)):
            value = resource.get(key)
            if not isinstance(value, str) or not value.strip() or len(value) > limit:
                raise ValueError(f"resource.{key} must be a nonempty string of at most {limit} characters")
        if not INSTANCE_ID.fullmatch(resource["instanceId"]):
            raise ValueError("each resource needs an explicit EC2 instance ID")
        if not re.fullmatch(r"[a-z]{2}(?:-[a-z]+)+-\d+", resource["region"]):
            raise ValueError("resource.region must be an AWS region name")
        if resource["profile"].startswith("-") or any(ord(c) < 32 for c in resource["profile"]):
            raise ValueError("resource.profile must be an AWS profile name")
        if "resource" in resource and (not isinstance(resource["resource"], str) or not resource["resource"].strip()):
            raise ValueError("resource.resource must be the exact board resource name")
        identity = (resource["profile"], resource["region"], resource["instanceId"])
        if identity in seen:
            raise ValueError("duplicate EC2 instance in the same profile and region")
        seen.add(identity)
    return resources


def load_claims(path):
    if not path:
        return None
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(data, dict) or not isinstance(data.get("claims"), dict):
        raise ValueError("the board must contain a claims object")
    return data["claims"]


def reservation(resource, claims):
    key = resource.get("resource")
    if claims is None or not key:
        return {"lease": "Lease unknown", "owner": None}
    claim = claims.get(key)
    if claim is None:
        return {"lease": "No reservation", "owner": None}
    if not isinstance(claim, dict):
        raise ValueError(f"invalid claim for {key}")
    holders = [claim[k] for k in ("worker", "thread") if isinstance(claim.get(k), str) and claim[k].strip()]
    if not holders or len(set(holders)) != 1:
        raise ValueError(f"missing or conflicting claim owner for {key}")
    return {"lease": "Reserved", "owner": holders[0]}


def describe(resources, timeout):
    groups = defaultdict(list)
    for resource in resources:
        groups[(resource["profile"], resource["region"])].append(resource["instanceId"])
    instances = {}
    for (profile, region), ids in groups.items():
        if STOP.is_set():
            return None
        command = ["aws", "ec2", "describe-instances", "--instance-ids", *ids,
                   "--profile", profile, "--region", region, "--output", "json",
                   "--no-cli-pager", "--cli-connect-timeout", "5", "--cli-read-timeout", str(timeout)]
        result = subprocess.run(command, capture_output=True, text=True, timeout=timeout,
                                env={**os.environ, "AWS_PAGER": "", "AWS_CLI_AUTO_PROMPT": "off"})
        if result.returncode:
            # Do not publish CLI output, account identifiers, or credential errors in panel state.
            raise RuntimeError(f"EC2 describe-instances failed (exit {result.returncode}, region {region})")
        payload = json.loads(result.stdout)
        if not isinstance(payload, dict) or not isinstance(payload.get("Reservations"), list):
            raise ValueError("EC2 returned an invalid Reservations collection")
        for group in payload["Reservations"]:
            if not isinstance(group, dict) or not isinstance(group.get("Instances"), list):
                raise ValueError("EC2 returned an invalid Instances collection")
            for instance in group["Instances"]:
                if not isinstance(instance, dict):
                    raise ValueError("EC2 returned an invalid instance")
                instance_id = instance.get("InstanceId")
                if instance_id in ids:
                    instances[(profile, region, instance_id)] = instance
    return instances


def snapshot(resources, claims, instances, timestamp=None):
    rows = {}
    running = 0
    reserved = 0
    unknown_leases = 0
    for index, resource in enumerate(resources):
        instance = instances.get((resource["profile"], resource["region"], resource["instanceId"]))
        state = instance.get("State", {}).get("Name", "unknown") if instance else "not returned"
        machine = instance.get("InstanceType", "Unknown type") if instance else "Unknown type"
        if not isinstance(state, str) or not isinstance(machine, str):
            raise ValueError("EC2 returned invalid instance state or type")
        lease = reservation(resource, claims)
        running += state == "running"
        reserved += lease["lease"] == "Reserved"
        unknown_leases += lease["lease"] == "Lease unknown"
        rows[str(index)] = {"instanceId": resource["instanceId"], "label": resource["label"],
                            "state": state, "type": machine, "region": resource["region"],
                            "tone": "success" if state == "running" else "neutral", **lease}
    moment = timestamp or datetime.now(timezone.utc).isoformat(timespec="seconds")
    summary = f"{running} running · {reserved} reserved" if not unknown_leases else f"{running} running · lease unknown"
    return {"instances": rows, "summary": summary,
            "updatedAt": moment, "updatedLabel": "Updated " + moment[11:19] + " UTC"}


def panel_spec(resources):
    initial = snapshot(resources, None, {}, "1970-01-01T00:00:00+00:00")
    initial.update(summary="Waiting for EC2", updatedAt=None, updatedLabel="Waiting for first update")
    for row in initial["instances"].values():
        row.update(state="Not checked", type="", lease="Lease unknown")
    elements = {
        "root": {"type": "Stack", "props": {"gap": "xs"},
                 "children": ["heading", "selector", *[f"instance-{i}" for i in range(len(resources))], "updated"]},
        "heading": {"type": "Stack", "props": {"direction": "row", "align": "center", "gap": "sm"},
                    "children": ["cloud", "summary"]},
        "cloud": {"type": "Icon", "props": {"name": "cloud", "size": "sm", "label": "EC2"}},
        "summary": {"type": "Text", "props": {"text": {"$state": "/live/summary"}, "kind": "label"}},
        "selector": {"type": "Select", "props": {"name": "instance", "label": "Instance",
                     "value": {"$bindState": "/selected"}, "options": [
                         {"label": r["label"], "value": str(i)} for i, r in enumerate(resources)]}},
        "updated": {"type": "Text", "props": {"text": {"$state": "/live/updatedLabel"}, "kind": "caption"}},
    }
    for i, _resource in enumerate(resources):
        key = f"instance-{i}"
        path = f"/live/instances/{i}"
        elements[key] = {"type": "Stack", "props": {"gap": "none"},
                         "visible": {"$state": "/selected", "eq": str(i)},
                         "children": [f"{key}-status", f"{key}-id"]}
        elements[f"{key}-status"] = {"type": "Stack", "props": {"direction": "row", "gap": "sm", "align": "center"},
                                     "children": [f"{key}-icon", f"{key}-state", f"{key}-lease"]}
        elements[f"{key}-icon"] = {"type": "Icon", "props": {"name": "server", "size": "sm"}}
        elements[f"{key}-state"] = {"type": "Badge", "props": {"label": {"$state": f"{path}/state"}, "tone": {"$state": f"{path}/tone"}}}
        elements[f"{key}-lease"] = {"type": "Text", "props": {"text": {"$state": f"{path}/lease"}, "kind": "caption"}}
        elements[f"{key}-id"] = {"type": "Text", "props": {"text": {"$state": f"{path}/instanceId"}, "kind": "caption"}}
    return {"action": "set", "spec": {"root": "root", "elements": elements,
                                        "state": {"selected": "0", "live": initial}}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, help="JSON with explicit instance IDs, profiles, and regions")
    parser.add_argument("--claims-file", help="exact codex-board.json supplied by the project; read only")
    parser.add_argument("--interval", type=int, default=30, help="seconds between reads, minimum 10")
    parser.add_argument("--timeout", type=int, default=15, help="seconds per AWS process, 1 to 60")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--once", action="store_true", help="emit one snapshot and exit")
    mode.add_argument("--panel-spec", action="store_true", help="emit the initial orchestration_panel payload; no AWS calls")
    args = parser.parse_args()
    if not 10 <= args.interval <= 3600 or not 1 <= args.timeout <= 60:
        parser.error("interval must be 10 to 3600 seconds; timeout must be 1 to 60 seconds")
    signal.signal(signal.SIGTERM, lambda *_: STOP.set())
    signal.signal(signal.SIGINT, lambda *_: STOP.set())
    try:
        resources = load_config(args.config)
        if args.panel_spec:
            print(json.dumps(panel_spec(resources), ensure_ascii=False))
            return 0
        while not STOP.is_set():
            # Read the reservation source on each tick. Do not manufacture or renew leases.
            claims = load_claims(args.claims_file)
            instances = describe(resources, args.timeout)
            if instances is None or STOP.is_set():
                return 0
            print(json.dumps(snapshot(resources, claims, instances), ensure_ascii=False, separators=(",", ":")), flush=True)
            if args.once or STOP.wait(args.interval):
                return 0
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"EC2 panel feed stopped: {error}", file=sys.stderr, flush=True)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
