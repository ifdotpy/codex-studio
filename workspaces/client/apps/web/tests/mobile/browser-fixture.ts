import type { ViteDevServer } from "vite";

export function fixtureServerOrigin(server: ViteDevServer): string {
  const origin = server.resolvedUrls?.local[0];
  if (!origin) throw new Error("Fixture server did not publish a local URL");
  return origin;
}

export async function until(
  check: () => boolean | Promise<boolean>,
  message: string,
  timeoutMs = 15_000,
) {
  const deadline = Date.now() + timeoutMs;
  while (!(await check())) {
    if (Date.now() > deadline) throw new Error(message);
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}
