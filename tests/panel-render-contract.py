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
    def test_full_height_three_card_canvas_and_small_surface(self):
        panel = {
            "version": 1,
            "html": """<main class="canvas">
              <section class="card"><strong>Plan</strong><span class="bottom"></span></section>
              <section class="card"><strong>Build</strong><span class="bottom"></span></section>
              <section class="card"><strong>Check</strong><span class="bottom"></span></section>
            </main>""",
            "css": """.canvas{height:150px;display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:10px;padding:10px;background:#050505}
              .card{position:relative;min-width:0;background:#404040;border-radius:6px;padding:8px}
              .bottom{position:absolute;bottom:0;left:8px;right:8px;height:3px;background:#b0b0b0}""",
        }
        result = render.render_panel(panel)
        self.assertTrue(result["layout"]["fits"])
        self.assertEqual([view["width"] for view in result["layout"]["viewports"]], [320, 640, 1000])
        self.assertTrue(all(view["contentHeight"] == 150 for view in result["layout"]["viewports"]))
        pixels = base64.b64decode(result["data_url"].split(",", 1)[1])
        self.assertEqual(struct.unpack(">II", pixels[16:24]), (1000, 150))
        self.assertEqual(png_pixel(pixels, 5, 145), (27, 27, 32))
        self.assertEqual(png_pixel(pixels, 5, 5), (27, 27, 32))
        for center in (170, 500, 830):
            self.assertEqual(png_pixel(pixels, center, 100), (64, 64, 64))
            # Tagged macOS PNGs can adjust light gray through the display profile.
            # A bright marker at the final card rows proves those rows were not cut.
            marker = png_pixel(pixels, center, 138)
            self.assertGreater(min(marker), 160)
            self.assertLessEqual(max(marker) - min(marker), 1)
        small = render.render_panel({
            "version": 2, "html": "<div style='width:100px;height:70px;background:#404040'>Small card</div>", "css": "",
        })
        small_pixels = base64.b64decode(small["data_url"].split(",", 1)[1])
        self.assertEqual(png_pixel(small_pixels, 50, 50), (64, 64, 64))
        self.assertEqual(png_pixel(small_pixels, 900, 100), (27, 27, 32))

    def test_legacy_request_and_finite_animation_lifecycle(self):
        with tempfile.TemporaryDirectory(prefix="codex-panel-legacy-") as folder:
            request = Path(folder) / "request.json"
            output = Path(folder) / "panel.png"
            request.write_text(json.dumps({
                "panel": {"html": "<div style='height:200px'>Legacy</div>", "css": "", "version": 1},
                "preview": str(render._ROOT / "web/dist/panel-preview.html"), "output": str(output),
                "width": 1000, "height": 150,
            }))
            environment = os.environ.copy()
            environment.pop("ELECTRON_RUN_AS_NODE", None)
            result = subprocess.run(render._electron_command() + ["--render-panel", str(request)],
                                    capture_output=True, env=environment, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
            self.assertEqual(output.read_bytes()[:8], b"\x89PNG\r\n\x1a\n")
        finite = render.render_panel({"html": "<div>Finite</div>",
                                     "css": "div{animation:fade 1s linear}@keyframes fade{from{opacity:0}to{opacity:1}}", "version": 1})
        self.assertTrue(all(view["animationSamples"] > 1 for view in finite["layout"]["viewports"]))

    def test_layout_rejects_scroll_clip_position_and_responsive_overflow(self):
        cases = {
            "tall": ("<div style='height:151px'>Tall</div>", "html,body{margin:0}", "outside-panel"),
            "nested clip": ("<div style='height:20px;overflow:hidden'><div style='height:100px'>Clipped</div></div>", "", "clipped-content"),
            "nested scroll": ("<div style='height:20px;overflow:auto'><div style='height:100px'>Scroll</div></div>", "", "clipped-content"),
            "zero clip": ("<div style='height:0;overflow:hidden'><div style='height:20px'>Clipped</div></div>", "", "clipped-content"),
            "negative nested clip": ("<div style='height:30px;overflow:hidden'><div style='height:20px;transform:translateY(-5px)'>Clipped</div></div>", "", "clipped-content"),
            "forced scrollbar": ("<div style='height:30px;overflow:scroll;scrollbar-width:auto!important'>Bar</div>", "", "visible-scrollbar"),
            "offscreen": ("<div style='position:absolute;top:-100px;height:20px'>Outside</div>", "", "outside-panel"),
            "negative pseudo": ("<div>Visible</div>", "div:after{content:'Hidden';position:absolute;top:-100px;height:20px}", "outside-pseudo"),
            "stroke": ("<svg width='120' height='150'><circle cx='60' cy='75' r='50' fill='none' stroke='red' stroke-width='100'/></svg>", "html,body{margin:0}", "outside-stroke"),
            "animation": ("<div>Moves outside</div>", "div{animation:move 2s linear infinite}@keyframes move{to{transform:translateY(180px)}}", "outside-panel"),
            "animation end": ("<div>Grows at end</div>", "div{height:20px;animation:grow 1s steps(1,end) forwards}@keyframes grow{to{height:200px}}", "outside-panel"),
            "narrow only": ("<div>Responsive</div>", "@media(max-width:400px){div{height:180px}}", "outside-panel"),
        }
        for name, (html, css, expected) in cases.items():
            with self.subTest(name=name):
                with self.assertRaises(render.PanelLayoutError) as caught:
                    render.render_panel({"html": html, "css": css, "version": 1})
                layout = caught.exception.layout
                self.assertFalse(layout["fits"])
                self.assertEqual([view["width"] for view in layout["viewports"]], [320, 640, 1000])
                self.assertTrue(any(item["kind"] == expected for view in layout["viewports"] for item in view["violations"]))
        self.assertTrue(layout["viewports"][-1]["fits"], "The desktop layout fits but the narrow layout must reject")

    def test_exact_height_svg_definitions_controls_and_legacy_capture(self):
        panel = {"html": """<div style='height:150px'><input value='A long value in a normal native field'>
          <svg width=20 height=20 viewBox='0 0 20 20'><defs><path id='unused' d='M0 0 L900 900'/></defs><circle cx=10 cy=10 r=8 /></svg></div>""",
          "css": "html,body{margin:0}", "version": 1}
        result = render.render_panel(panel)
        self.assertTrue(result["layout"]["fits"])
        scaled = render.render_panel({"html": "<div style='position:absolute;top:0;left:0;width:200px;height:300px;transform:scale(.4);transform-origin:0 0'>Scaled content</div>",
                                      "css": "html,body{margin:0}", "version": 1})
        self.assertTrue(scaled["layout"]["fits"])
        line = render.render_panel({"html": "<svg width='100%' height='150'><line x1='0' y1='75' x2='100%' y2='75' stroke='red' stroke-width='2' stroke-linecap='butt'/></svg>",
                                    "css": "html,body{margin:0}svg{display:block}", "version": 1})
        self.assertTrue(line["layout"]["fits"])
        panel["html"] = "<div style='height:200px'>Old oversized panel</div>"
        old = render.render_panel(panel, strict_layout=False)
        self.assertFalse(old["layout"]["fits"])
        self.assertTrue(old["data_url"].startswith("data:image/png;base64,"))

    def test_real_image_theme_and_exact_revision(self):
        panel = {"version": 17, "html": "<h1>Build status</h1><button data-callback=retry>Retry</button>",
                 "css": "", "callbacks": [{"id": "retry", "label": "Retry"}]}
        original_command = render._electron_command

        def mutate_after_snapshot():
            panel.update(version=18, html="<div class='tile'>Changed later</div>", css=".tile{width:100px;height:80px;background:red}")
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
        self.assertEqual(png_pixel(image, 900, 100), (27, 27, 32))
        self.assertNotEqual(png_pixel(image, 15, 40), (27, 27, 32))
        second = render.render_panel(panel)
        self.assertEqual(second["version"], 18)
        # macOS can encode display-profile RGB values in its tagged PNG.
        # Assert the rendered color, rather than an unconverted sRGB byte value.
        red, green, blue = png_pixel(base64.b64decode(second["data_url"].split(",", 1)[1]), 50, 60)
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
                "html": f'''<section class="proof">Surface</section>
                    <script src="{url}"></script><script>document.querySelector('.proof').style.background='red'</script>
                    <img src="{url}" onerror="document.body.style.background='red'">
                    <iframe src="{url}"></iframe><form action="{url}"><button data-callback="retry">Retry</button></form>''',
                "css": f'''.proof{{background:rgb(20,90,180);width:200px;height:80px}}body{{background-image:url({url})}}
                    </style><script>document.body.style.background='red'</script>''',
                "callbacks": [{"id": "retry", "label": "</script><script>alert(1)</script>"}],
            })
            image = base64.b64decode(result["data_url"].split(",", 1)[1])
            red, green, blue = png_pixel(image, 100, 60)
            self.assertLess(red, 80)
            self.assertGreater(green, 60)
            self.assertLess(green, 120)
            self.assertGreater(blue, 150)
            self.assertEqual(png_pixel(image, 900, 140), (27, 27, 32))
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

    def test_real_rejected_layout_preserves_previous_version_and_callbacks(self):
        agent = self.runtime.prepare(self.lead())
        request = {"id": "real-valid", "params": {
            "threadId": agent["threadId"], "callId": "real-valid", "tool": "orchestration_panel",
            "arguments": {"action": "set", "html": "<button data-callback=retry>Retry</button>",
                          "callbacks": [{"id": "retry", "label": "Retry"}]},
        }}
        self.runtime.dynamic(request)
        self.assertTrue(self.runtime.server.responses[-1]["result"]["success"])
        previous = self.runtime.panel(agent["id"])
        request.update(id="real-too-tall")
        request["params"].update(callId="real-too-tall", arguments={"action": "set", "html": "<div style='height:200px'>Too tall</div>"})
        self.runtime.dynamic(request)
        response = self.runtime.server.responses[-1]["result"]
        self.assertFalse(response["success"])
        self.assertIn("previous panel is unchanged", response["contentItems"][0]["text"])
        self.assertEqual(self.runtime.panel(agent["id"]), previous)

    def test_real_dynamic_result_contains_immutable_png_and_cached_replay(self):
        agent = self.runtime.prepare(self.lead())
        request = {"id": "real-render", "params": {
            "threadId": agent["threadId"], "callId": "real-render",
            "tool": "orchestration_panel", "arguments": {
                "action": "set", "html": "<h1>Official build</h1><progress max=8 value=3></progress>",
                "css": "html{background:#1b1b20}",
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
        self.assertEqual(png_pixel(raw, 900, 100), (27, 27, 32))
        self.assertEqual(self.runtime.panel(agent["id"])["version"], 1)
        self.runtime.panel_action(agent["id"], {"action": "set", "html": "New revision"})
        # A retry receives the exact original image bytes after a newer write.
        self.runtime.dynamic(request)
        self.assertEqual(self.runtime.server.responses[-1]["result"], first)
        self.assertEqual(self.runtime.panel(agent["id"])["version"], 2)


if __name__ == "__main__":
    unittest.main()
