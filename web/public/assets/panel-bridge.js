// Only this trusted function runs in the opaque iframe. Agent scripts and event
// attributes are removed before its CSP nonce is created.
function bridge(config) {
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
bridge(
  JSON.parse(
    decodeURIComponent(
      document.currentScript?.getAttribute("data-config") || "%7B%7D",
    ),
  ),
);
