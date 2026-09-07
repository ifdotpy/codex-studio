// Executed inside the isolated panel frame, with no Node or workspace access.
// Keep geometry checks independent of overflow styles: clipping is not a fit.
module.exports = function measurePanelLayout() {
  const width = innerWidth;
  const height = innerHeight;
  const tolerance = 0.5;
  const violations = [];
  let left = 0,
    top = 0,
    right = width,
    bottom = height;
  const record = (value) => {
    if (violations.length < 20) violations.push(value);
  };
  const label = (element) =>
    element.localName + (element.id ? `#${element.id}` : "");
  const unrenderedSVG =
    "defs,clipPath,mask,pattern,marker,linearGradient,radialGradient,symbol";
  const rendered = (element) =>
    element.getClientRects().length &&
    !element.closest(unrenderedSVG) &&
    getComputedStyle(element).display !== "none";
  const bounds = (rect, element, kind) => {
    if (!rect.width && !rect.height) return;
    left = Math.min(left, rect.left);
    top = Math.min(top, rect.top);
    right = Math.max(right, rect.right);
    bottom = Math.max(bottom, rect.bottom);
    if (
      rect.top < -tolerance ||
      rect.left < -tolerance ||
      rect.bottom > height + tolerance ||
      rect.right > width + tolerance
    ) {
      record({
        kind,
        element: label(element),
        top: rect.top,
        left: rect.left,
        bottom: rect.bottom,
        right: rect.right,
      });
    }
  };
  for (const element of document.querySelectorAll("*")) {
    if (!rendered(element)) continue;
    const rect = element.getBoundingClientRect();
    bounds(rect, element, "outside-panel");
    const style = getComputedStyle(element);
    const parent = element.parentElement;
    if (parent && !parent.matches("html,body") && style.position !== "fixed") {
      const parentStyle = getComputedStyle(parent);
      if (style.position !== "absolute" || parentStyle.position !== "static") {
        const clip = parent.getBoundingClientRect();
        const clippedY =
          parentStyle.overflowY !== "visible" &&
          (rect.top < clip.top - tolerance ||
            rect.bottom > clip.bottom + tolerance);
        const clippedX =
          parentStyle.overflowX !== "visible" &&
          (rect.left < clip.left - tolerance ||
            rect.right > clip.right + tolerance);
        if (clippedX || clippedY)
          record({
            kind: "clipped-content",
            element: label(parent),
            contentHeight: Math.ceil(
              Math.max(clip.bottom, rect.bottom) - Math.min(clip.top, rect.top),
            ),
            availableHeight: Math.ceil(clip.height),
            contentWidth: Math.ceil(
              Math.max(clip.right, rect.right) - Math.min(clip.left, rect.left),
            ),
            availableWidth: Math.ceil(clip.width),
          });
      }
    }
    if (
      element instanceof SVGGeometryElement &&
      style.stroke !== "none" &&
      Number(style.strokeOpacity) !== 0
    ) {
      const halfStroke = Number.parseFloat(style.strokeWidth) / 2;
      const matrix = element.getScreenCTM();
      if (halfStroke > 0 && matrix) {
        const fixed = style.vectorEffect === "non-scaling-stroke";
        let x = halfStroke * (fixed ? 1 : Math.hypot(matrix.a, matrix.c));
        let y = halfStroke * (fixed ? 1 : Math.hypot(matrix.b, matrix.d));
        if (
          element instanceof SVGLineElement &&
          style.strokeLinecap === "butt"
        ) {
          // Butt caps stop at endpoints. Only the perpendicular stroke extends
          // beyond the line; inflating both axes rejects valid edge-to-edge charts.
          let dx = element.x2.baseVal.value - element.x1.baseVal.value;
          let dy = element.y2.baseVal.value - element.y1.baseVal.value;
          if (fixed)
            [dx, dy] = [
              matrix.a * dx + matrix.c * dy,
              matrix.b * dx + matrix.d * dy,
            ];
          const length = Math.hypot(dx, dy);
          const nx = length ? (-dy / length) * halfStroke : 0;
          const ny = length ? (dx / length) * halfStroke : 0;
          x = Math.abs(fixed ? nx : matrix.a * nx + matrix.c * ny);
          y = Math.abs(fixed ? ny : matrix.b * nx + matrix.d * ny);
        }
        bounds(
          {
            left: rect.left - x,
            top: rect.top - y,
            right: rect.right + x,
            bottom: rect.bottom + y,
            width: rect.width + 2 * x,
            height: rect.height + 2 * y,
          },
          element,
          "outside-stroke",
        );
      }
    }
    // Native single-line controls scroll their internal editor. That is not an
    // agent-created scroll area; their outside border still must fit the panel.
    if (
      !element.matches("html,input,select,button,progress,meter") &&
      element.namespaceURI === "http://www.w3.org/1999/xhtml"
    ) {
      const yClipped =
        style.overflowY !== "visible" &&
        element.scrollHeight > element.clientHeight + 1;
      const xClipped =
        style.overflowX !== "visible" &&
        element.scrollWidth > element.clientWidth + 1;
      if (
        style.scrollbarWidth !== "none" &&
        [style.overflowX, style.overflowY].includes("scroll")
      )
        record({ kind: "visible-scrollbar", element: label(element) });
      if (xClipped || yClipped)
        record({
          kind: "clipped-content",
          element: label(element),
          contentHeight: element.scrollHeight,
          availableHeight: element.clientHeight,
          contentWidth: element.scrollWidth,
          availableWidth: element.clientWidth,
        });
    }
  }
  // Text ranges expose overflow from nowrap, line-clamp, and zero-height boxes.
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const element = node.parentElement;
    if (
      !node.textContent.trim() ||
      !element ||
      !rendered(element) ||
      element.closest("script,style,textarea,select") ||
      element.namespaceURI !== "http://www.w3.org/1999/xhtml"
    )
      continue;
    const range = document.createRange();
    range.selectNodeContents(node);
    for (const rect of range.getClientRects())
      bounds(rect, element, "outside-text");
  }
  const root = document.documentElement;
  right = Math.max(right, root.scrollWidth);
  bottom = Math.max(bottom, root.scrollHeight);
  if (
    root.scrollHeight > height + tolerance ||
    root.scrollWidth > width + tolerance
  )
    record({
      kind: "document-overflow",
      contentHeight: root.scrollHeight,
      contentWidth: root.scrollWidth,
    });
  return {
    width,
    height,
    contentHeight: Math.ceil(bottom - top),
    contentWidth: Math.ceil(right - left),
    fits: violations.length === 0,
    violations,
  };
};
