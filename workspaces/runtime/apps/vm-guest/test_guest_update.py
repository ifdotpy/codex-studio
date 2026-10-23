"""In-place guest updates: the root broker's path choice, signature, file writes and restarts."""
from __future__ import annotations

import asyncio
import base64
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
from common import GuestError
import guest_update
from guest_update import ADMIN_UNIT, GUEST_UNIT, LAYR_UNIT, UpdateHandlers, signature

KEY = "ab" * 32
RECORD = {"codex": "0.160.1", "claude": "2.1.291", "pnpm": "10.0.0", "script": "c" * 64}
OLD = {"vm/guest/service.py": b"old service\n", "vm/guest/share_bridge.py": b"old bridge\n",
       "vm/layr/src/service.rs": b"old layr\n", "bin/codex-process-supervisor": b"supervisor\n",
       ".agents/skills/codex-workspace/SKILL.md": b"skill\n"}


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Runner:
    """The install script and systemctl: recorded, never run."""

    def __init__(self) -> None:
        self.calls: list[tuple[list[str], dict[str, str]]] = []
        self.install_code = 0

    async def __call__(self, argv, env, timeout):
        self.calls.append((list(argv), dict(env)))
        if argv[-1].endswith("install.sh"):
            return self.install_code, "install output"
        return 0, ""


class GuestUpdateTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="guest-update-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.opt, self.state = root / "opt", root / "state"
        for name, data in OLD.items():
            (self.opt / name).parent.mkdir(parents=True, exist_ok=True)
            (self.opt / name).write_bytes(data)
        self.key = root / "update-key"
        self.key.write_text(KEY + "\n")
        self.record = root / "guest-payload.json"
        self.record.write_text(json.dumps(RECORD))
        self.busy = 0
        self.runner = Runner()
        self.handlers = UpdateHandlers(self.state, root / "layr", opt=self.opt, key_path=self.key,
                                       record_path=self.record, busy=lambda: self.busy, runner=self.runner)

    def files(self, changes: dict[str, bytes] | None = None) -> tuple[dict[str, str], dict[str, bytes]]:
        content = {**OLD, **(changes or {})}
        return {name: sha(data) for name, data in content.items()}, content

    def call(self, method: str, params: dict) -> dict:
        return asyncio.run(self.handlers.dispatch("request", method, params, None))

    def plan(self, manifest: dict[str, str], record: dict | None = None) -> dict:
        return self.call("update.plan", {"files": manifest, "payload": record or RECORD})

    def stage(self, operation: str, data: bytes, part: int = 4) -> None:
        for offset in range(0, len(data), part):
            self.call("update.stage", {"operationId": operation, "sha256": sha(data), "offset": offset,
                                       "data": base64.b64encode(data[offset:offset + part]).decode()})

    def apply(self, operation: str, manifest: dict[str, str], key: str = KEY) -> dict:
        return self.call("update.apply", {"operationId": operation, "files": manifest,
                                          "signature": signature(key.encode(), operation, manifest)})

    def test_the_path_follows_what_changed(self) -> None:
        manifest, _ = self.files()
        self.assertEqual(self.plan(manifest)["path"], "current")
        manifest, _ = self.files({"vm/guest/service.py": b"new service\n"})
        chosen = self.plan(manifest)
        self.assertEqual((chosen["path"], chosen["changed"]), ("in_place", ["vm/guest/service.py"]))
        self.assertEqual(chosen["restarts"], [GUEST_UNIT, ADMIN_UNIT])
        manifest, _ = self.files({"vm/layr/src/service.rs": b"new layr\n"})
        self.assertEqual(self.plan(manifest)["restarts"], [LAYR_UNIT])
        manifest, _ = self.files({"vm/guest/share_bridge.py": b"new bridge\n"})
        self.assertEqual(self.plan(manifest)["restarts"], ["codex-studio-share-bridge.service"])
        manifest, _ = self.files({"vm/guest/codex-studio-layr-admin.service": b"[Unit]\n"})
        self.assertEqual(self.plan(manifest)["restarts"], ["codex-studio-layr-admin.service"])

    def test_provider_parts_an_older_guest_or_new_versions_need_a_reboot(self) -> None:
        # The process supervisor owns the running provider processes.
        manifest, _ = self.files({"bin/codex-process-supervisor": b"new supervisor\n"})
        chosen = self.plan(manifest)
        self.assertEqual((chosen["path"], chosen["reboot"]), ("reboot", ["bin/codex-process-supervisor"]))
        manifest, _ = self.files({"vm/guest/service.py": b"new service\n"})
        self.assertEqual(self.plan(manifest, {**RECORD, "claude": "2.1.292"})["path"], "reboot")
        self.record.unlink()
        self.assertEqual(self.plan(manifest)["path"], "reboot")
        self.record.write_text(json.dumps(RECORD))
        self.key.unlink()
        self.assertIn("no update key", " ".join(self.plan(manifest)["reasons"]))

    def test_only_the_mac_key_installs_files(self) -> None:
        manifest, content = self.files({"vm/guest/service.py": b"root code\n"})
        self.stage("op-1", content["vm/guest/service.py"])
        with self.assertRaises(GuestError) as caught:
            self.apply("op-1", manifest, key="cd" * 32)
        self.assertEqual(caught.exception.code, "forbidden")
        with self.assertRaises(GuestError):
            self.call("update.apply", {"operationId": "op-1", "files": manifest, "signature": "x"})
        self.key.unlink()
        with self.assertRaises(GuestError) as caught:
            self.apply("op-1", manifest)
        self.assertEqual(caught.exception.code, "update_unsupported")
        self.assertEqual((self.opt / "vm/guest/service.py").read_bytes(), OLD["vm/guest/service.py"])
        self.assertEqual(self.runner.calls, [])

    def test_an_in_place_update_writes_installs_and_restarts_after_the_drain(self) -> None:
        manifest, content = self.files({"vm/guest/service.py": b"new service, longer than one part\n"})
        self.stage("op-2", content["vm/guest/service.py"])
        self.busy = 2  # this apply and one other broker request

        async def run() -> dict:
            status = await self.handlers.dispatch("apply", "update.apply", {
                "operationId": "op-2", "files": manifest,
                "signature": signature(KEY.encode(), "op-2", manifest)}, None)
            self.assertEqual(status["stage"], "restart_pending")
            self.assertEqual((self.opt / "vm/guest/service.py").read_bytes(), content["vm/guest/service.py"])
            install = self.runner.calls[-1]
            self.assertEqual((install[0][-1].endswith("install.sh"), install[1]),
                             (True, {"STUDIO_DEFER_RESTARTS": "1"}))
            await asyncio.sleep(1.5)
            self.assertEqual(len(self.runner.calls), 1)  # still busy: no restart
            self.busy = 0
            await asyncio.wait_for(asyncio.gather(*self.handlers.tasks), 5)
            return status

        asyncio.run(run())
        restart = self.runner.calls[-1][0]
        # The broker restarts itself, so a separate systemd unit runs the restart.
        self.assertEqual(restart[0], "/usr/bin/systemd-run")
        self.assertEqual(restart[-3:], ["restart", GUEST_UNIT, ADMIN_UNIT])
        self.assertEqual(self.call("update.status", {"operationId": "op-2"})["stage"], "restart_scheduled")
        self.handlers.finish_restarts(10 ** 10)  # the new broker process
        self.assertEqual(self.call("update.status", {"operationId": "op-2"})["stage"], "complete")
        self.assertEqual(self.plan(manifest)["path"], "current")

    def test_a_share_bridge_restart_runs_at_once_and_completes(self) -> None:
        manifest, content = self.files({"vm/guest/share_bridge.py": b"new bridge\n"})
        self.stage("op-3", content["vm/guest/share_bridge.py"])
        self.apply("op-3", manifest)
        asyncio.run(asyncio.wait_for(asyncio.gather(*self.handlers.tasks), 5)) if self.handlers.tasks else None
        self.assertEqual(self.call("update.status", {"operationId": "op-3"})["stage"], "complete")

    def test_a_reboot_file_or_a_missing_part_writes_nothing(self) -> None:
        manifest, content = self.files({"vm/guest/service.py": b"new service\n",
                                       "bin/codex-process-supervisor": b"new supervisor\n"})
        self.stage("op-4", content["vm/guest/service.py"])
        with self.assertRaises(GuestError) as caught:
            self.apply("op-4", manifest)
        self.assertEqual(caught.exception.code, "reboot_required")
        manifest, _ = self.files({"vm/guest/service.py": b"never staged\n"})
        with self.assertRaises(GuestError):
            self.apply("op-5", manifest)
        self.assertEqual((self.opt / "vm/guest/service.py").read_bytes(), OLD["vm/guest/service.py"])
        self.assertEqual(self.runner.calls, [])

    def test_a_retry_after_a_failed_install_finishes_the_same_work(self) -> None:
        manifest, content = self.files({"vm/layr/src/service.rs": b"new layr\n"})
        self.stage("op-6", content["vm/layr/src/service.rs"])
        self.runner.install_code = 1
        with self.assertRaises(GuestError) as caught:
            self.apply("op-6", manifest)
        self.assertEqual(caught.exception.code, "install_failed")
        # The file is written, so a new plan sees no change; the retry still installs and restarts.
        self.runner.install_code = 0

        async def retry() -> dict:
            status = await self.handlers.dispatch("retry", "update.apply", {
                "operationId": "op-6", "files": manifest,
                "signature": signature(KEY.encode(), "op-6", manifest)}, None)
            await asyncio.wait_for(asyncio.gather(*self.handlers.tasks), 5)
            return status

        status = asyncio.run(retry())
        self.assertEqual((status["changed"], status["restarts"]), (["vm/layr/src/service.rs"], [LAYR_UNIT]))
        commands = [argv for argv, _ in self.runner.calls]
        self.assertEqual([argv[-1].endswith("install.sh") for argv in commands], [True, True, False])
        # The broker requires the layr daemon, so a separate unit restarts it after the reply.
        self.assertEqual(commands[-1][0], "/usr/bin/systemd-run")
        self.assertEqual(commands[-1][-2:], ["restart", LAYR_UNIT])

    def test_a_new_operation_after_a_failed_install_finishes_the_earlier_work(self) -> None:
        manifest, content = self.files({"vm/guest/service.py": b"new service\n"})
        self.stage("op-8", content["vm/guest/service.py"])
        self.runner.install_code = 1
        with self.assertRaises(GuestError):
            self.apply("op-8", manifest)
        # The file is in place, but its install and restarts are still to do.
        chosen = self.plan(manifest)
        self.assertEqual((chosen["path"], chosen["changed"]), ("in_place", ["vm/guest/service.py"]))
        self.runner.install_code = 0
        status = self.apply("op-9", manifest)
        self.assertEqual((status["changed"], status["restarts"]),
                         (["vm/guest/service.py"], [GUEST_UNIT, ADMIN_UNIT]))
        self.assertEqual(self.call("update.status", {"operationId": "op-8"})["stage"], "superseded")
        self.assertEqual([argv[-1].endswith("install.sh") for argv, _ in self.runner.calls][:2], [True, True])

    def test_paths_and_parts_are_bounded(self) -> None:
        for name in ("/etc/passwd", "../etc/passwd", "vm/guest/../../etc", "vm//guest"):
            with self.subTest(name=name), self.assertRaises(GuestError):
                self.plan({name: "a" * 64})
        with self.assertRaises(GuestError):
            self.call("update.stage", {"operationId": "op-7", "sha256": "a" * 64, "offset": 9,
                                       "data": base64.b64encode(b"x").decode()})
        with self.assertRaises(GuestError):
            self.call("update.stage", {"operationId": "../op", "sha256": "a" * 64, "offset": 0, "data": ""})
        self.assertEqual(guest_update.classify(["vm/guest/codex-studio-grow.service"])["restarts"], [])


if __name__ == "__main__":
    unittest.main()
