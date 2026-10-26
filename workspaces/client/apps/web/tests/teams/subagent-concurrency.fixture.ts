declare global {
  interface Window {
    concurrencyStorageFixture: (leadId: string) => string;
  }
}

/** Installed as a standalone init script in every page that inspects saved settings. */
export function installConcurrencyStorageFixture() {
  window.concurrencyStorageFixture = (leadId) => {
    const prefix = "studio-agent-mode:";
    const key = Object.keys(localStorage).find((entry) => {
      if (!entry.startsWith(prefix)) return false;
      // The renderer writes these scope keys in this same fixture context.
      const scope: string[] = JSON.parse(entry.slice(prefix.length));
      return scope[2] === leadId;
    });
    if (!key) throw Error("Missing stored setting");
    return key;
  };
}
