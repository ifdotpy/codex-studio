/** @typedef {{ stdout: NodeJS.ReadableStream | null, once: Function, off: Function }} FixtureChild */

/**
 * Resolve the port printed by a fixture server, with shared timeout and exit handling.
 * @param {FixtureChild} child
 * @param {{ timeoutMs?: number; label?: string; diagnostics?: () => string }} [options]
 */
export function waitForFixturePort(
  child,
  { timeoutMs = 30_000, label = "Fixture", diagnostics = () => "" } = {},
) {
  if (!child.stdout) throw new Error(`${label} fixture has no stdout stream`);
  return new Promise((resolve, reject) => {
    let output = "";
    const cleanup = () => {
      clearTimeout(timer);
      child.stdout?.off("data", onData);
      child.off("error", onError);
      child.off("close", onClose);
    };
    const fail = (error) => {
      cleanup();
      reject(error);
    };
    const onData = (chunk) => {
      output += String(chunk);
      const newline = output.indexOf("\n");
      if (newline < 0) return;
      const line = output.slice(0, newline).trim();
      const port = Number(line);
      if (!Number.isInteger(port) || port < 1 || port > 65_535) {
        fail(new Error(`${label} fixture printed an invalid port: ${line}`));
        return;
      }
      cleanup();
      resolve(port);
    };
    const onError = (error) => fail(error);
    const onClose = (code, signal) =>
      fail(
        new Error(
          `${label} fixture exited before printing its port (code ${code ?? "null"}, signal ${signal ?? "none"}): ${output.trim()} ${diagnostics()}`.trim(),
        ),
      );
    const timer = setTimeout(
      () =>
        fail(
          new Error(
            `${label} fixture did not print its port within ${timeoutMs}ms: ${output.trim()} ${diagnostics()}`.trim(),
          ),
        ),
      timeoutMs,
    );
    child.stdout.on("data", onData);
    child.once("error", onError);
    child.once("close", onClose);
  });
}
