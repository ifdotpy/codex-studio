// Hidden native-check fixture. config is emitted by its Rust owner.
(async () => {
  if (window.__studioAttachmentPrivacyStarted) return;
  window.__studioAttachmentPrivacyStarted = true;
  const host = window.__TAURI__;
  const internal = window.__TAURI_INTERNALS__;
  const summary = {
    passed: false,
    rustDelivered: 0,
    rawLeakCount: 0,
    allowed: 0,
    denied: 0,
    fakeRegistry: 0,
    callbackIntercept: false,
    registerApiAbsent: false,
    foreignDenied: 0,
    unsafeControl: false,
    foreignAdmissions: 0,
    foreignBridgeAccepted: false,
    brokerReady: false,
    brokerDenied: 0,
  };
  const foreignFrames = async () => {
    const rows = [
      { name: "studio", origin: config.foreign.origin },
      { name: "opaque", origin: "null", sandbox: "allow-scripts" },
      { name: "unsafe-control", origin: location.origin },
    ].map((row) => ({ ...row, frame: document.createElement("iframe") }));
    let resolve;
    const ready = new Promise((done) => {
      resolve = done;
    });
    const receive = (event) => {
      const row = rows.find(
        (row) =>
          event.source === row.frame.contentWindow &&
          event.origin === row.origin,
      );
      if (!row) return;
      if (
        event.data?.kind === "studio-native-frame-response" &&
        event.data.id === `${row.frame.name}:borrow` &&
        !event.data.error
      ) {
        summary.foreignBridgeAccepted = true;
      }
      if (
        event.data?.kind !== config.foreign.resultKind ||
        event.data.id !== row.frame.name ||
        row.result
      )
        return;
      row.result = event.data;
      if (rows.every((row) => row.result)) resolve();
    };
    window.addEventListener("message", receive);
    for (const row of rows) {
      row.frame.hidden = true;
      row.frame.name = `${config.foreign.namePrefix}${row.name}`;
      if (row.sandbox) row.frame.setAttribute("sandbox", row.sandbox);
      if (row.name === "studio")
        row.frame.src = `${row.origin}${config.foreign.documentPath}`;
      else row.frame.srcdoc = "<!doctype html><html><body></body></html>";
      document.body.append(row.frame);
    }
    await ready;
    summary.foreignDenied = rows.filter(
      (row) =>
        row.name !== "unsafe-control" &&
        row.result.ownIPC === false &&
        row.result.parentIPC === false &&
        row.result.unsafeAccepted === false,
    ).length;
    const control = rows.find((row) => row.name === "unsafe-control").result;
    summary.unsafeControl =
      control.parentIPC === true && control.unsafeAccepted === true;
    return () => {
      window.removeEventListener("message", receive);
      for (const row of rows) row.frame.remove();
    };
  };
  const observe = (value) => {
    try {
      if (JSON.stringify(value).includes(config.markerPrefix))
        summary.rawLeakCount += 1;
    } catch {
      /* A non-JSON callback is not a native drag payload. */
    }
  };
  let sequence = 0;
  const request = async (method, params) => {
    const id = `${method}:${++sequence}`;
    const value = await host.core.invoke("client_request", {
      request: { id, method, params },
    });
    if (value.id !== id || value.error || !Object.hasOwn(value, "result"))
      throw Error("fixture_rpc");
    return value.result;
  };
  try {
    const disposeFrames = await foreignFrames();
    // Spy on the real callback map as well as normal listeners. The map is
    // reachable in this pinned framework even though runCallback is immutable.
    const callbacks = internal.callbacks;
    const originalGet = callbacks.get.bind(callbacks);
    Object.defineProperty(callbacks, "get", {
      configurable: true,
      value(id) {
        const callback = originalGet(id);
        return (
          callback &&
          ((value) => {
            observe(value);
            return callback(value);
          })
        );
      },
    });
    summary.callbackIntercept = true;
    const channel = new host.core.Channel();
    channel.onmessage = observe;
    const lease = await request(config.broker.connect, { callback: channel });
    if (typeof lease?.leaseId !== "string" || !lease.leaseId)
      throw Error("broker_unavailable");
    summary.brokerReady = true;
    for (const forbidden of [
      { event: config.events[0] },
      { topic: config.events[1] },
      { window: "main" },
      { context: "foreign-fixture-context" },
    ]) {
      try {
        await request(config.broker.connect, {
          callback: channel,
          ...forbidden,
        });
      } catch {
        summary.brokerDenied += 1;
      }
    }
    const plugin = window.__TAURI_EVENT_PLUGIN_INTERNALS__;
    summary.registerApiAbsent = typeof plugin?.registerListener !== "function";
    if (!summary.registerApiAbsent)
      throw Error("framework_registration_changed");
    // An attacker can forge every JS registry entry. This cannot create the
    // separate Rust js_event_listeners entry required before native eval.
    let registry = window[config.listenerObject];
    if (!registry) {
      registry = Object.create(null);
      Object.defineProperty(window, config.listenerObject, { value: registry });
    }
    const fakeHandler = internal.transformCallback(observe);
    for (const event of config.events) {
      if (!registry[event])
        Object.defineProperty(registry, event, { value: Object.create(null) });
      Object.defineProperty(registry[event], config.forgedListenerId, {
        configurable: true,
        value: { target: { kind: "Any" }, handlerId: fakeHandler },
      });
      summary.fakeRegistry += 1;
    }
    // Neither a JavaScript replacement nor an added plugin emit helper may
    // become a native registration path. Pinned immutable properties reject it.
    Reflect.set(window, config.emitterFunction, observe);
    Reflect.set(internal, "runCallback", (_id, value) => observe(value));
    if (plugin && typeof plugin.emit !== "function") plugin.emit = observe;
    const handler = internal.transformCallback(observe);
    for (const event of config.events) {
      for (const target of [
        { kind: "Any" },
        { kind: "Window", label: "main" },
        { kind: "Webview", label: "main" },
        { kind: "WebviewWindow", label: "main" },
      ]) {
        try {
          await host.core.invoke("plugin:event|listen", {
            event,
            target,
            handler,
          });
          summary.allowed += 1;
        } catch {
          summary.denied += 1;
        }
      }
    }
    const receipt = await request(config.begin, {});
    await request(config.broker.disconnect, { leaseId: lease.leaseId });
    disposeFrames();
    summary.rustDelivered = receipt.rustDelivered;
    summary.foreignAdmissions = receipt.foreignAdmissions;
    summary.passed =
      receipt.rustDelivered === config.emissions &&
      summary.rawLeakCount === 0 &&
      summary.allowed === 0 &&
      summary.foreignDenied === 2 &&
      summary.unsafeControl &&
      summary.foreignAdmissions === 1 &&
      !summary.foreignBridgeAccepted &&
      summary.brokerReady &&
      summary.brokerDenied === 4;
    if (summary.rawLeakCount) summary.failure = config.failures.raw;
    else if (summary.allowed) summary.failure = config.failures.admission;
    else if (
      summary.foreignDenied !== 2 ||
      !summary.unsafeControl ||
      summary.foreignAdmissions !== 1 ||
      summary.foreignBridgeAccepted
    )
      summary.failure = config.failures.foreign;
  } catch {
    summary.failure = config.failures.setup;
  }
  await request(config.finish, summary);
})().catch(() => {
  document.title = "Native attachment privacy fixture did not record a result";
});
