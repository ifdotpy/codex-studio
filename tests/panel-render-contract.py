#!/usr/bin/env python3
"""Real hidden Electron panels, no backend, user profile, or model requests."""

import base64
import http.server
import importlib.util
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import codex_panel_render as render


def png_pixel(data, x, y):
    """Read Electron's PNG pixels with the standard library only."""
    position = 8
    compressed = bytearray()
    while position < len(data):
        size = struct.unpack(">I", data[position:position + 4])[0]
        kind = data[position + 4:position + 8]
        content = data[position + 8:position + 8 + size]
        if kind == b"IHDR":
            width, height, depth, color, _, _, interlace = struct.unpack(">IIBBBBB", content)
        elif kind == b"IDAT":
            compressed.extend(content)
        position += size + 12
    assert depth == 8 and color in (2, 6) and interlace == 0
    channels = 4 if color == 6 else 3
    stride = width * channels
    raw = zlib.decompress(compressed)
    prior = bytearray(stride)
    for row in range(y + 1):
        offset = row * (stride + 1)
        filter_type = raw[offset]
        current = bytearray(raw[offset + 1:offset + stride + 1])
        for index in range(stride):
            left = current[index - channels] if index >= channels else 0
            above = prior[index]
            upper_left = prior[index - channels] if index >= channels else 0
            if filter_type == 1:
                predictor = left
            elif filter_type == 2:
                predictor = above
            elif filter_type == 3:
                predictor = (left + above) // 2
            elif filter_type == 4:
                value = left + above - upper_left
                distances = [abs(value - left), abs(value - above), abs(value - upper_left)]
                predictor = [left, above, upper_left][distances.index(min(distances))]
            else:
                assert filter_type == 0
                predictor = 0
            current[index] = (current[index] + predictor) % 256
        prior = current
    return tuple(prior[x * channels:x * channels + 3])


class RenderContract(unittest.TestCase):
    def test_real_image_theme_and_exact_revision(self):
        panel = {"version": 17, "html": "<h1>Build status</h1><button data-callback=retry>Retry</button>",
                 "css": "", "callbacks": [{"id": "retry", "label": "Retry"}]}
        original_command = render._electron_command

        def mutate_after_snapshot():
            panel.update(version=18, html="<h1>Changed later</h1>", css="html{background:red}")
            return original_command()

        with tempfile.TemporaryDirectory(prefix="codex-panel-profile-test-") as folder:
            state = Path(folder) / "state"
            profile = Path(folder) / "user-profile"
            profile.mkdir()
            (profile / "sentinel").write_text("Existing user profile")
            with patch.object(render, "_electron_command", mutate_after_snapshot), patch.dict(os.environ, {
                "CODEX_AGENTS_STATE_DIR": str(state), "CODEX_DESKTOP_PROFILE": str(profile),
            }):
                result = render.render_panel(panel)
            self.assertFalse(state.exists(), "Screenshot must not start the workspace backend")
            self.assertEqual(list(profile.iterdir()), [profile / "sentinel"])
            self.assertEqual((profile / "sentinel").read_text(), "Existing user profile")
        self.assertEqual((result["version"], result["width"], result["height"]), (17, 1000, 150))
        image = base64.b64decode(result["data_url"].split(",", 1)[1])
        self.assertEqual(png_pixel(image, 900, 100), (24, 25, 28))
        self.assertNotEqual(png_pixel(image, 25, 58), (24, 25, 28))
        second = render.render_panel(panel)
        self.assertEqual(second["version"], 18)
        # macOS can encode display-profile RGB values in its tagged PNG.
        # Assert the rendered color, rather than an unconverted sRGB byte value.
        red, green, blue = png_pixel(base64.b64decode(second["data_url"].split(",", 1)[1]), 900, 100)
        self.assertGreater(red, 200)
        self.assertLess(green, 70)
        self.assertLess(blue, 70)
        self.assertNotEqual(second["data_url"], result["data_url"])

    def test_sanitized_scripts_css_injection_and_no_network(self):
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                requests.append(self.path)
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"document.body.style.background='red'")

            def log_message(self, *_args):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        url = f"http://127.0.0.1:{server.server_port}/must-not-load"
        try:
            result = render.render_panel({
                "version": 2,
                "html": f'''<script src="{url}"></script><script>document.body.style.background='red'</script>
                    <img src="{url}" onerror="document.body.style.background='red'">
                    <iframe src="{url}"></iframe><form action="{url}"><button data-callback="retry">Retry</button></form>''',
                "css": f'''html,body{{background:rgb(20,90,180);margin:0;height:100%}}body{{background-image:url({url})}}
                    </style><script>document.body.style.background='red'</script>''',
                "callbacks": [{"id": "retry", "label": "</script><script>alert(1)</script>"}],
            })
            image = base64.b64decode(result["data_url"].split(",", 1)[1])
            red, green, blue = png_pixel(image, 900, 100)
            self.assertLess(red, 80)
            self.assertGreater(green, 60)
            self.assertLess(green, 120)
            self.assertGreater(blue, 150)
            self.assertEqual(requests, [])
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_timeout_kills_owned_process_and_removes_private_profile(self):
        paths = []
        processes = []
        real_popen = subprocess.Popen

        def stuck_process(command, **kwargs):
            request = Path(command[-1])
            paths.append(request.parent)
            child = real_popen([sys.executable, "-c", "import time;time.sleep(60)"], **kwargs)
            processes.append(child)
            return child

        with patch.object(render.subprocess, "Popen", stuck_process), patch.object(render, "_TIMEOUT", .15):
            with self.assertRaisesRegex(render.PanelRenderError, "timed out"):
                render.render_panel({"html": "<p>Timeout fixture</p>", "css": "", "version": 1})
        self.assertTrue(paths)
        self.assertTrue(all(not path.exists() for path in paths))
        self.assertEqual(processes[0].returncode, -signal.SIGKILL)
        with self.assertRaises(ProcessLookupError):
            os.kill(processes[0].pid, 0)

    def test_saturation_and_invalid_input_fail_visibly(self):
        with self.assertRaisesRegex(render.PanelRenderError, "HTML"):
            render.render_panel({"version": 1})
        self.assertTrue(render._RENDERERS.acquire(blocking=False))
        self.assertTrue(render._RENDERERS.acquire(blocking=False))
        try:
            with patch.object(render, "_QUEUE_TIMEOUT", .01), self.assertRaisesRegex(render.PanelRenderError, "busy"):
                render.render_panel({"html": "<p>Busy fixture</p>"})
        finally:
            render._RENDERERS.release()
            render._RENDERERS.release()


spec = importlib.util.spec_from_file_location(
    "panel_render_workspace_fixture", Path(__file__).with_name("workspace-contract.py"),
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class DynamicRenderContract(unittest.TestCase):
    setUp = fixture.WorkspaceContract.setUp
    tearDown = fixture.WorkspaceContract.tearDown
    lead = fixture.WorkspaceContract.lead

    def test_real_dynamic_result_contains_immutable_png_and_cached_replay(self):
        agent = self.runtime.prepare(self.lead())
        request = {"id": "real-render", "params": {
            "threadId": agent["threadId"], "callId": "real-render",
            "tool": "orchestration_panel", "arguments": {
                "action": "set", "html": "<h1>Official build</h1><progress max=8 value=3></progress>",
                "css": "html{background:#18191c}",
            },
        }}
        self.runtime.dynamic(request)
        first = self.runtime.server.responses[-1]["result"]
        self.assertTrue(first["success"], first)
        image = next(item for item in first["contentItems"] if item["type"] == "inputImage")
        metadata = json.loads(first["contentItems"][0]["text"])
        self.assertEqual(metadata["render"], {"width": 1000, "height": 150, "version": 1})
        raw = base64.b64decode(image["imageUrl"].split(",", 1)[1])
        self.assertEqual(raw[:8], b"\x89PNG\r\n\x1a\n")
        self.assertEqual(png_pixel(raw, 900, 100), (24, 25, 28))
        self.assertEqual(self.runtime.panel(agent["id"])["version"], 1)
        self.runtime.panel_action(agent["id"], {"action": "set", "html": "New revision"})
        # A retry receives the exact original image bytes after a newer write.
        self.runtime.dynamic(request)
        self.assertEqual(self.runtime.server.responses[-1]["result"], first)
        self.assertEqual(self.runtime.panel(agent["id"])["version"], 2)


if __name__ == "__main__":
    unittest.main()
