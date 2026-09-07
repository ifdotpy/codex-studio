// Runs inside the opaque frame. The frame retains its 150px validation viewport;
// only the host's visible area changes with the measured content.
function observePanelContent(channel) {
  let frame = 0;
  let previous = -1;
  const measure = () => {
    frame = 0;
    const root = document.getElementById("panel-root") || document.body;
    let bottom = 0;
    for (const node of root.querySelectorAll("*")) {
      if (
        node.matches("script,style,meta,link") ||
        !node.getClientRects().length
      )
        continue;
      const rect = node.getBoundingClientRect();
      if (rect.width || rect.height) bottom = Math.max(bottom, rect.bottom);
    }
    // HTML panels can contain text without an enclosing element.
    for (const node of root.childNodes) {
      if (node.nodeType !== Node.TEXT_NODE || !node.textContent?.trim())
        continue;
      const range = document.createRange();
      range.selectNodeContents(node);
      bottom = Math.max(bottom, range.getBoundingClientRect().bottom);
    }
    const height = Math.min(
      150,
      Math.max(
        0,
        Math.ceil(
          bottom +
            (Number.parseFloat(getComputedStyle(root).paddingBottom) || 0),
        ),
      ),
    );
    if (height === 0 || height === previous) return;
    previous = height;
    parent.postMessage({ type: "panel-size", channel, height }, "*");
  };
  const schedule = () => {
    if (!frame) frame = requestAnimationFrame(measure);
  };
  const resize = new ResizeObserver(schedule);
  const observe = () => {
    resize.observe(document.body);
    document.body.querySelectorAll("*").forEach((node) => {
      if (!node.matches("script,style")) resize.observe(node);
    });
    schedule();
  };
  new MutationObserver(observe).observe(document.body, {
    subtree: true,
    childList: true,
    characterData: true,
    attributes: true,
  });
  addEventListener("resize", schedule);
  document.addEventListener("load", schedule, true);
  void document.fonts.ready.then(schedule);
  observe();
}

// Only this trusted function runs in the opaque iframe. Agent scripts and event
// attributes are removed before its CSP nonce is created.
function bridge(config) {
  // A full-size outer wrapper is the panel canvas, not a separate card.
  // Keep its backdrop continuous with the host while preserving nested surfaces.
  const roots = Array.from(document.body.children).filter(
    (node) => !node.matches("script,style"),
  );
  const canvas =
    roots.length === 1 && roots[0].matches("div,main") ? roots[0] : null;
  if (canvas) {
    const originalStyle = canvas.getAttribute("style");
    let normalized = false;
    const syncCanvas = () => {
      const rect = canvas.getBoundingClientRect();
      const fills =
        Math.abs(rect.left) < 0.5 &&
        Math.abs(rect.top) < 0.5 &&
        rect.width >= innerWidth - 0.5 &&
        rect.height >= innerHeight - 0.5;
      if (fills === normalized) return;
      normalized = fills;
      if (fills)
        canvas.style.setProperty("background", "transparent", "important");
      else if (originalStyle === null) canvas.removeAttribute("style");
      else canvas.setAttribute("style", originalStyle);
    };
    syncCanvas();
    const resize = new ResizeObserver(syncCanvas);
    resize.observe(document.documentElement);
    resize.observe(canvas);
  }
  const callbacks = new Map(config.callbacks.map((item) => [item.id, item]));
  let busy = true;
  let locked = [];
  const originalDisabled = new WeakSet();
  document
    .querySelectorAll(":disabled")
    .forEach((node) => originalDisabled.add(node));
  document.querySelectorAll('input[type="file"]').forEach((node) => {
    node.disabled = true;
    originalDisabled.add(node);
  });
  const controls = () =>
    document.querySelectorAll(
      "button, input[type=submit], input[type=button], input[type=image]",
    );
  const action = (node) =>
    node.getAttribute("data-callback") ||
    node.form?.getAttribute("data-callback") ||
    node.closest("form")?.getAttribute("data-callback") ||
    "";
  const update = () =>
    controls().forEach((node) => {
      const id = action(node);
      node.disabled =
        originalDisabled.has(node) ||
        busy ||
        !callbacks.has(id) ||
        locked.includes(id);
    });
  update();
  addEventListener("message", (event) => {
    if (
      event.source !== parent ||
      event.data?.channel !== config.channel ||
      event.data?.type !== "panel-state"
    )
      return;
    busy = event.data.busy !== false;
    locked = Array.isArray(event.data.locked) ? event.data.locked : [];
    update();
  });
  const send = (id, form, submitter) => {
    const callback = callbacks.get(id);
    if (busy || !callback || locked.includes(id)) return;
    const values = Object.create(null);
    if (form) {
      if (!HTMLFormElement.prototype.reportValidity.call(form)) return;
      const data = new FormData(form, submitter || undefined);
      for (const name of callback.fields || []) {
        const entries = data.getAll(name);
        if (entries.some((value) => typeof value !== "string")) return;
        if (entries.length) values[name] = entries;
      }
    }
    busy = true;
    update();
    parent.postMessage(
      { type: "panel-callback", channel: config.channel, callback: id, values },
      "*",
    );
  };
  document.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!event.isTrusted || !(event.target instanceof HTMLFormElement)) return;
    const submitter = event.submitter;
    send(
      submitter?.getAttribute("data-callback") ||
        event.target.getAttribute("data-callback") ||
        "",
      event.target,
      submitter,
    );
  });
  document.addEventListener("click", (event) => {
    if (!event.isTrusted || !(event.target instanceof Element)) return;
    const button = event.target.closest(
      "button, input[type=button], input[type=submit], input[type=image]",
    );
    if (!button || button.disabled) return;
    if (button.form && (button.type === "submit" || button.type === "image"))
      return;
    const id = button.getAttribute("data-callback") || "";
    if (id) {
      event.preventDefault();
      send(id, button.form);
    }
  });
  parent.postMessage({ type: "panel-ready", channel: config.channel }, "*");
}
const config = JSON.parse(
  decodeURIComponent(
    document.currentScript?.getAttribute("data-config") || "%7B%7D",
  ),
);
observePanelContent(config.channel);
if (!config.layoutOnly) bridge(config);
