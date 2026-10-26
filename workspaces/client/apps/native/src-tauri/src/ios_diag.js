// Diagnostic geometry sample for STUDIO_SHELL_DIAG=1 (ios_shell.rs).
(() => {
  const rect = (node) => {
    if (!node) return null;
    const r = node.getBoundingClientRect();
    return [r.x, r.y, r.width, r.height].map((v) => Math.round(v * 10) / 10);
  };
  const root = document.getElementById("root");
  const style = root ? getComputedStyle(root) : null;
  const html = document.documentElement;
  const vv = window.visualViewport;
  const probe = document.createElement("div");
  probe.style.cssText =
    "position:fixed;visibility:hidden;padding:env(safe-area-inset-top) env(safe-area-inset-right) env(safe-area-inset-bottom) env(safe-area-inset-left)";
  document.body?.appendChild(probe);
  const env = getComputedStyle(probe).padding;
  probe.remove();
  return JSON.stringify({
    url: location.href,
    inner: [innerWidth, innerHeight],
    client: [html.clientWidth, html.clientHeight],
    scroll: [scrollX, scrollY, html.scrollWidth, html.scrollHeight],
    vv: vv ? [vv.width, vv.height, vv.offsetTop, vv.pageTop, vv.scale] : null,
    dpr: devicePixelRatio,
    env,
    nativeVars: ["top", "right", "bottom", "left"].map((s) =>
      html.style.getPropertyValue(`--native-safe-${s}`),
    ),
    shell: window.__studioNativeViewport?.state ?? null,
    attrs: [...html.attributes]
      .map((a) => a.name)
      .filter((n) => n.startsWith("data-")),
    inlineStyle: html.getAttribute("style"),
    root: rect(root),
    rootPadding: style ? style.padding : null,
    sidebar: rect(document.getElementById("sidebar")),
    composer: rect(document.getElementById("composer")),
    edgeRail: rect(document.querySelector(".edge-rail, [data-edge-rail-side]")),
    workspaceRail:
      document.querySelector(".workspace")?.getAttribute("data-edge-rail") ??
      null,
    active: document.activeElement?.tagName ?? null,
  });
})();
