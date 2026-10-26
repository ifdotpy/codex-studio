/// <reference lib="webworker" />
import type { ShellManifest } from "../../scripts/build-manifests.ts";

declare const self: ServiceWorkerGlobalScope & { STUDIO_SHELL: ShellManifest };

/** The renderer build writes this manifest from the shared shell contract. */
export function readShellManifest(worker: string): ShellManifest {
  const serialized = worker.match(/^self\.STUDIO_SHELL = (.*);/)?.[1];
  if (!serialized)
    throw new Error("Built service worker must declare its shell manifest");
  return JSON.parse(serialized);
}

export function workerShellBuild() {
  return self.STUDIO_SHELL.build;
}

export function workerIsActivated() {
  return self.registration.active?.state === "activated";
}

export function installLoadingWorker() {
  const studioNativeCacheOpen = caches.open.bind(caches);
  caches.open = async (...args) => {
    const response = await fetch("/test-cache-storage").catch(() => null);
    if (response && !response.ok) throw new Error("Storage unavailable");
    return studioNativeCacheOpen(...args);
  };
  self.addEventListener("activate", (event) =>
    event.waitUntil(
      fetch("/test-worker-activated?build=" + self.STUDIO_SHELL.build),
    ),
  );
}

declare global {
  interface Window {
    mobileLoadingUiBootstrap?: string;
  }
}
