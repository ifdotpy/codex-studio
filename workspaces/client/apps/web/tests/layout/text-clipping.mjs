// Serializable browser function. Call page.evaluate(detectTextClipping, {scene, profile}).
export function detectTextClipping({ scene, profile }) {
  const findings = [];
  const path = (el) => {
    const parts = [];
    while (el && el !== document.body) {
      if (el.id) {
        parts.unshift("#" + CSS.escape(el.id));
        break;
      }
      let part = el.tagName.toLowerCase();
      const siblings = Array.from(el.parentElement?.children || []).filter(
        (x) => x.tagName === el.tagName,
      );
      if (siblings.length > 1)
        part += `:nth-of-type(${siblings.indexOf(el) + 1})`;
      parts.unshift(part);
      el = el.parentElement;
    }
    return parts.join(" > ");
  };
  const visible = (el) => {
    if (el.closest(".sr-only,.mantine-VisuallyHidden-root")) return false;
    const s = getComputedStyle(el),
      r = el.getBoundingClientRect();
    if (!el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true }))
      return false;
    if (s.clip !== "auto" || s.clipPath !== "none") return false;
    for (let a = el.parentElement; a; a = a.parentElement) {
      const c = getComputedStyle(a),
        ar = a.getBoundingClientRect();
      if (
        ["auto", "scroll", "hidden", "clip"].includes(c.overflowY) &&
        (r.bottom <= ar.top || r.top >= ar.bottom)
      )
        return false;
    }
    return (
      s.visibility === "visible" &&
      s.display !== "none" &&
      +s.opacity !== 0 &&
      r.width > 0 &&
      r.height > 0 &&
      r.bottom > 0 &&
      r.right > 0 &&
      r.top < innerHeight &&
      r.left < innerWidth
    );
  };
  const hasFullText = (el, text) => {
    for (let a = el; a; a = a.parentElement) {
      if (
        [a.title, a.getAttribute("aria-label")].some((x) => x?.includes(text))
      )
        return true;
      const refs = (a.getAttribute("aria-describedby") || "").split(/\s+/);
      if (
        refs.some((id) =>
          document.getElementById(id)?.textContent.includes(text),
        )
      )
        return true;
    }
    return false;
  };
  const add = (el, text, reason, details = {}) => {
    const r = el.getBoundingClientRect();
    findings.push({
      scene,
      profile,
      selector: path(el),
      text,
      rect: { x: r.x, y: r.y, width: r.width, height: r.height },
      reason,
      ...details,
    });
  };
  for (const el of document.querySelectorAll("body *")) {
    if (
      !visible(el) ||
      el.closest("svg,script,style,textarea,select,option,.xterm")
    )
      continue;
    const nodes = Array.from(el.childNodes).filter(
      (n) => n.nodeType === Node.TEXT_NODE && n.textContent.trim(),
    );
    if (!nodes.length) continue;
    const text = nodes.map((n) => n.textContent.trim()).join(" "),
      s = getComputedStyle(el);
    const ranges = nodes.flatMap((n) => {
      const range = document.createRange();
      range.selectNodeContents(n);
      return Array.from(range.getClientRects());
    });
    // Scroll containers expose their content through scrolling. Hidden/clip ancestors cannot.
    const reasons = new Set();
    let visibleTextRects = ranges;
    let horizontalResolved = false;
    const clippingAncestors = [];
    for (let a = el; a; a = a.parentElement) {
      const previousReasons = reasons.size;
      const cs = getComputedStyle(a),
        ar = a.getBoundingClientRect();
      if (
        ["inline", "contents"].includes(cs.display) ||
        !ar.width ||
        !ar.height
      )
        continue;
      if (["auto", "scroll"].includes(cs.overflowX)) horizontalResolved = true;
      if (
        !horizontalResolved &&
        ["hidden", "clip"].includes(cs.overflowX) &&
        ranges.some(
          (t) =>
            t.left < ar.left + a.clientLeft - 1 ||
            t.right > ar.left + a.clientLeft + a.clientWidth + 1,
        )
      ) {
        if (cs.textOverflow === "ellipsis") {
          horizontalResolved = true;
          if (!hasFullText(el, text)) reasons.add("ellipsis-without-full-text");
        } else if (!["auto", "scroll"].includes(cs.overflowX))
          reasons.add("horizontal-clipping-without-ellipsis");
      }
      if (["hidden", "clip"].includes(cs.overflowY)) {
        if (a === el && el.scrollHeight > el.clientHeight + 1)
          reasons.add("vertical-scroll-clipping");
        if (
          visibleTextRects.some(
            (t) =>
              t.top < ar.top + a.clientTop - 1 ||
              t.bottom > ar.top + a.clientTop + a.clientHeight + 1,
          )
        )
          reasons.add("vertical-text-clipping");
      }
      if (["auto", "scroll"].includes(cs.overflowY)) {
        visibleTextRects = visibleTextRects
          .map((t) => ({
            ...t,
            top: Math.max(t.top, ar.top + a.clientTop),
            bottom: Math.min(t.bottom, ar.top + a.clientTop + a.clientHeight),
          }))
          .filter((t) => t.bottom > t.top);
      }
      if (reasons.size > previousReasons)
        clippingAncestors.push({
          selector: path(a),
          display: cs.display,
          overflowX: cs.overflowX,
          overflowY: cs.overflowY,
          clientWidth: a.clientWidth,
          clientHeight: a.clientHeight,
          x: ar.x,
          y: ar.y,
          width: ar.width,
          height: ar.height,
        });
      if (
        ["auto", "scroll"].includes(cs.overflowY) &&
        ranges.every((t) => t.bottom <= ar.top || t.top >= ar.bottom)
      )
        break;
    }
    if (
      ["hidden", "clip"].includes(s.overflowX) &&
      el.scrollWidth > el.clientWidth + 1
    ) {
      if (s.textOverflow === "ellipsis") {
        if (!hasFullText(el, text)) reasons.add("ellipsis-without-full-text");
      } else reasons.add("horizontal-clipping-without-ellipsis");
    }
    if (
      ["auto", "scroll"].includes(s.overflowY) &&
      el.scrollHeight > el.clientHeight + 1
    ) {
      add(el, text, "vertical-scroll-overflow", {
        intentional: true,
        justification: "The container exposes the full text through scrolling.",
      });
    }
    const clamped = Number(s.webkitLineClamp) > 0;
    for (const reason of reasons)
      add(el, text, reason, {
        intentional: clamped && hasFullText(el, text),
        clippingAncestors,
        clientHeight: el.clientHeight,
        scrollHeight: el.scrollHeight,
        lineHeight: s.lineHeight,
      });
    const control = el.closest('button,label,[role="button"]');
    if (control && ranges.length) {
      const t = ranges[0],
        x = t.left + t.width / 2,
        y = t.top + t.height / 2;
      if (x > 0 && x < innerWidth && y > 0 && y < innerHeight) {
        const hit = document.elementFromPoint(x, y);
        // Modal backdrops intentionally disable the scene below them.
        const modal = Array.from(document.querySelectorAll('[role="dialog"]'))
          .filter(visible)
          .at(-1);
        let inScroll = true;
        for (let a = el.parentElement; a; a = a.parentElement) {
          const c = getComputedStyle(a),
            ar = a.getBoundingClientRect();
          if (
            ["auto", "scroll", "hidden", "clip"].includes(c.overflowY) &&
            (y < ar.top || y > ar.bottom)
          )
            inScroll = false;
        }
        const popup = hit?.closest(
          '[role="listbox"],[role="menu"],.mantine-Combobox-dropdown,.mantine-Menu-dropdown',
        );
        const stickyHeader = hit?.closest(
          ".mantine-Modal-header,.mantine-Drawer-header",
        );
        let scrollableStickyHeader = false;
        if (
          stickyHeader &&
          getComputedStyle(stickyHeader).position === "sticky"
        ) {
          for (let a = control.parentElement; a; a = a.parentElement) {
            if (
              ["auto", "scroll"].includes(getComputedStyle(a).overflowY) &&
              a.scrollHeight > a.clientHeight + 1 &&
              a.contains(stickyHeader)
            )
              scrollableStickyHeader = true;
          }
        }
        if (
          inScroll &&
          hit &&
          !control.contains(hit) &&
          !hit.contains(el) &&
          (!modal || modal.contains(el))
        )
          add(el, text, "covered-control-text", {
            cover: path(hit),
            intentional: !!popup || scrollableStickyHeader,
            ...(popup
              ? {
                  justification: "The open popup covers the controls below it.",
                }
              : scrollableStickyHeader
                ? {
                    justification:
                      "The sticky header covers content in the same scroll container.",
                  }
                : {}),
          });
      }
    }
  }
  return findings;
}
