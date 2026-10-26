/** Static shell manifest injected by the renderer build into the service worker. */
export interface ShellManifest {
  build: string;
  initial: string[];
  allowed: string[];
}

/** Build identity written to studio-renderer.json by Vite. */
export interface RendererManifest {
  version: 1;
  build: string;
  apiSchema: string;
}
