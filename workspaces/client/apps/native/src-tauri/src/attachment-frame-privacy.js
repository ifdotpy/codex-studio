// This probe receives no Tauri key or API from the native fixture.
(() => {
  if (window === window.parent || !window.name.startsWith(config.namePrefix))
    return;
  if (
    location.href !== "about:srcdoc" &&
    !location.href.endsWith(config.documentPath)
  )
    return;
  if (window.__studioForeignFrameProbeStarted) return;
  window.__studioForeignFrameProbeStarted = true;
  const id = window.name;
  const result = {
    kind: config.resultKind,
    id,
    ownIPC: !!(
      window.__TAURI__?.core?.invoke ||
      window.__TAURI_INTERNALS__?.invoke ||
      window.ipc
    ),
    parentIPC: false,
    unsafeAccepted: false,
    keylessPacketSent: false,
  };
  try {
    result.parentIPC = typeof parent.__TAURI__?.core?.invoke === "function";
  } catch {
    /* A foreign or opaque frame cannot read the shell. */
  }
  const requestId = `${id}:direct`;
  const request = { id: requestId, method: config.attempt, params: {} };
  try {
    // Omitting the key tests the real parser without triggering the pinned
    // invalid-key logger, which prints the expected private key.
    const ipc = window.webkit?.messageHandlers?.ipc;
    if (ipc) {
      ipc.postMessage(
        JSON.stringify({
          cmd: "client_request",
          callback: 4294967293,
          error: 4294967292,
          payload: { request },
          options: { headers: { Origin: "tauri://localhost" } },
        }),
      );
      result.keylessPacketSent = true;
    }
  } catch {
    /* Missing native message handlers are also an expected boundary. */
  }
  // The real compatibility bridge must reject an unmapped frame or a borrowed
  // context. The new native entry must not install that bridge at all.
  parent.postMessage(
    {
      kind: "studio-native-frame-request",
      id: `${id}:borrow`,
      method: "service.request",
      params: {
        handle: "foreign-fixture-context",
        method: config.attempt,
        params: {},
      },
    },
    "*",
  );
  const finish = () => parent.postMessage(result, "*");
  if (result.parentIPC) {
    // Unsandboxed srcdoc is an explicit unsafe same-origin positive control.
    parent.__TAURI__.core
      .invoke("client_request", { request })
      .then((value) => {
        result.unsafeAccepted =
          value?.id === requestId && value?.result?.accepted === true;
        finish();
      }, finish);
  } else finish();
})();
