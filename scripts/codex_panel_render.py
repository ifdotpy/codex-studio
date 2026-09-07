"""Render one immutable panel revision without connecting to the live workspace."""

import base64
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import tempfile
import threading

from codex_panel import validate_panel_spec


_RENDERERS = threading.BoundedSemaphore(2)
_ROOT = Path(__file__).resolve().parent.parent
_TIMEOUT = 15
_QUEUE_TIMEOUT = 30
WIDTH = 1000
HEIGHT = 150


class PanelRenderError(RuntimeError):
    """The renderer did not produce a verified PNG for the submitted revision."""


class PanelLayoutError(PanelRenderError):
    """Measured content needs more space or hides content behind a clip."""

    def __init__(self, layout):
        self.layout = layout
        failed = next(view for view in layout["viewports"] if not view["fits"])
        self.actual_height = failed["contentHeight"]
        self.actual_width = failed["contentWidth"]
        self.max_height = layout["maxHeight"]
        self.width = failed["width"]
        violation = failed["violations"][0]
        detail = violation["kind"]
        if detail == "clipped-content":
            detail = (f"{violation['element']} clips {violation['contentHeight']}px of content "
                      f"inside {violation['availableHeight']}px")
        super().__init__(
            f"Panel does not fit at {self.width}px width: content {self.actual_width}x{self.actual_height}px, "
            f"maximum {self.width}x{self.max_height}px ({detail}). The previous panel is unchanged. "
            "Reduce or rearrange the content; scrolling and clipping are not allowed."
        )


def _electron_command():
    """Use the source Electron or this package, never another user's app state."""
    configured = os.environ.get("CODEX_STUDIO_ELECTRON")
    if configured:
        executable = Path(configured).expanduser().resolve()
        if not executable.is_file():
            raise PanelRenderError("CODEX_STUDIO_ELECTRON does not name an executable.")
        return [str(executable)]
    source = _ROOT / "desktop/node_modules/electron/dist"
    candidates = [source / "Electron.app/Contents/MacOS/Electron", source / "electron"]
    for executable in candidates:
        if executable.is_file():
            return [str(executable), str(_ROOT / "desktop")]
    packaged = _ROOT.parent.parent / "MacOS/Codex Studio"
    if packaged.is_file():
        return [str(packaged)]
    raise PanelRenderError("The panel renderer needs the Codex Studio desktop package or desktop Electron dependency.")


def render_panel(panel, *, strict_layout=True):
    """Return {data_url, width, height, version, layout} for the exact passed panel.

    No database or runtime locks may be held by the caller. At most two renderer
    processes run concurrently, with up to 30 seconds to acquire a slot and
    15 seconds to finish each capture. Saturation or failure raises PanelRenderError;
    Strict mode rejects overflow before a caller commits its panel write.
    strict_layout=False can capture an old panel and return its failed metrics.
    """
    if not isinstance(panel, dict):
        raise PanelRenderError("A panel with HTML or a structured spec is required.")
    structured = panel.get("format") == "json-render" or "spec" in panel
    if structured:
        try:
            validate_panel_spec(panel.get("spec"))
        except ValueError as error:
            raise PanelRenderError(str(error)) from error
        if panel.get("html", "") or panel.get("css", ""):
            raise PanelRenderError("A structured panel cannot also contain HTML or CSS.")
    elif not isinstance(panel.get("html"), str):
        raise PanelRenderError("A panel with HTML or a structured spec is required.")
    # Make the revision independent of subsequent caller mutations before wait.
    try:
        immutable = json.loads(json.dumps(panel, ensure_ascii=False))
    except (TypeError, ValueError) as error:
        raise PanelRenderError("The panel is not JSON serializable.") from error
    immutable.setdefault("html", "")
    immutable.setdefault("css", "")
    if structured:
        immutable["format"] = "json-render"
    if not isinstance(immutable["html"], str) or not isinstance(immutable["css"], str):
        raise PanelRenderError("Panel HTML and CSS must be strings.")
    if len(immutable["html"].encode("utf-8")) > 131072 or len(immutable["css"].encode("utf-8")) > 32768:
        raise PanelRenderError("The panel exceeds the render size limit.")
    if not _RENDERERS.acquire(timeout=_QUEUE_TIMEOUT):
        raise PanelRenderError("Both panel renderers stayed busy for 30 seconds. No new panel was accepted. Try again after the queue clears.")
    try:
        preview = _ROOT / "web/dist/panel-preview.html"
        if not preview.is_file():
            raise PanelRenderError("The panel preview is not built. Build or update Codex Studio.")
        command = _electron_command()
        with tempfile.TemporaryDirectory(prefix="codex-panel-render-") as directory:
            folder = Path(directory)
            request_path = folder / "request.json"
            output = folder / "panel.png"
            request_path.write_text(json.dumps({
                "panel": immutable, "preview": str(preview), "output": str(output),
                "width": WIDTH, "height": HEIGHT,
                "strictLayout": strict_layout,
            }, ensure_ascii=False), encoding="utf-8")
            request_path.chmod(0o600)
            environment = os.environ.copy()
            for key in ("ELECTRON_RUN_AS_NODE", "NODE_OPTIONS", "NODE_EXTRA_CA_CERTS"):
                environment.pop(key, None)
            environment["ELECTRON_DISABLE_SECURITY_WARNINGS"] = "true"
            try:
                with (folder / "stderr.log").open("wb") as errors:
                    process = subprocess.Popen(
                        command + ["--render-panel", str(request_path)],
                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=errors,
                        env=environment, start_new_session=True,
                    )
                    try:
                        status = process.wait(timeout=_TIMEOUT)
                    except subprocess.TimeoutExpired as error:
                        # Kill only the process group created for this render,
                        # including Chromium children, before deleting its profile.
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
                        raise PanelRenderError("The panel renderer timed out after 15 seconds.") from error
                if status:
                    detail = (folder / "stderr.log").read_text(errors="replace")[-1600:].strip()
                    raise PanelRenderError(f"The panel renderer exited with code {status}: {detail}")
                layout = json.loads(Path(str(output) + ".json").read_text(encoding="utf-8"))
                if not isinstance(layout.get("fits"), bool) or len(layout.get("viewports", [])) != 3:
                    raise PanelRenderError("The panel renderer did not return complete layout measurements.")
                if strict_layout and not layout["fits"]:
                    raise PanelLayoutError(layout)
                data = output.read_bytes()
            except OSError as error:
                raise PanelRenderError(f"The panel renderer could not produce its image: {error}") from error
            if len(data) < 24 or data[:8] != b"\x89PNG\r\n\x1a\n":
                raise PanelRenderError("The panel renderer did not return a PNG image.")
            dimensions = struct.unpack(">II", data[16:24])
            if dimensions != (WIDTH, HEIGHT):
                raise PanelRenderError(f"The panel renderer returned unexpected dimensions {dimensions}.")
            return {
                "data_url": "data:image/png;base64," + base64.b64encode(data).decode("ascii"),
                "width": WIDTH, "height": HEIGHT, "version": immutable.get("version"),
                "layout": layout,
            }
    finally:
        _RENDERERS.release()
