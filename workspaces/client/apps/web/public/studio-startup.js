(() => {
  const updateUrl = new URL(window.location.href);
  if (updateUrl.searchParams.has("studio-update")) {
    updateUrl.searchParams.delete("studio-update");
    window.history.replaceState(null, "", updateUrl);
  }
  const notice = (text) => {
    const status = document.getElementById("studio-startup-status");
    const help = document.getElementById("studio-startup-help");
    if (status) status.textContent = text;
    if (help)
      help.textContent =
        "Check the connection to your computer, then reload Studio.";
  };
  const timeout = setTimeout(
    () => notice("Studio is taking longer to open."),
    12000,
  );
  window.addEventListener(
    "error",
    (event) => {
      if (event.target instanceof HTMLScriptElement || event.error) {
        clearTimeout(timeout);
        notice("Studio could not open.");
      }
    },
    true,
  );
  window.addEventListener("unhandledrejection", () =>
    notice("Studio could not open."),
  );
  if (
    window.codexDesktop ||
    !window.isSecureContext ||
    !("serviceWorker" in navigator) ||
    !document.querySelector('meta[name="studio-build"]')
  )
    return;
  // The Update action activates the waiting worker with skipWaiting, which
  // also claims existing tabs. The worker keeps the previous shell available.
  window.addEventListener(
    "load",
    () => {
      navigator.serviceWorker
        .register("/studio-sw.js", { scope: "/", updateViaCache: "none" })
        .then((registration) => {
          // The in-app Update action gets this registration, waits for worker
          // activation, and only then navigates to the network-fetched shell.
          let checked = Date.now();
          const update = () => {
            if (
              document.visibilityState !== "visible" ||
              Date.now() - checked < 60000
            )
              return;
            checked = Date.now();
            void registration.update().catch(() => {});
          };
          document.addEventListener("visibilitychange", update);
          window.addEventListener("online", update);
        })
        .catch(() => {});
    },
    { once: true },
  );
})();
