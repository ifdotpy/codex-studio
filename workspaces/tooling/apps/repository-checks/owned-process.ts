import { spawn } from "node:child_process";
import { appendFileSync } from "node:fs";
import {
  GATE_COMMAND_TIMEOUT_MS,
  PROCESS_STOP_GRACE_MS,
} from "./gate-config.ts";

// Every command owns a new Unix process group, including nested pnpm/Cargo children.
// Never signal the caller's group, and reap owned children before reporting terminal state.
export function runLogged(
  command,
  args,
  cwd,
  log,
  environment = process.env,
  { signal, timeoutMs = GATE_COMMAND_TIMEOUT_MS, onOutput } = {},
) {
  return new Promise((resolve) => {
    const group = process.platform !== "win32";
    const child = spawn(command, args, {
      cwd,
      detached: group,
      stdio: ["ignore", "pipe", "pipe"],
      env: environment,
    });
    let output = "";
    let interrupted = null;
    let error = null;
    let escalation = null;
    let cleanupDone = Promise.resolve();
    const send = (sig) => {
      if (!child.pid) return false;
      try {
        if (group) process.kill(-child.pid, sig);
        else child.kill(sig);
        return true;
      } catch (err) {
        if (err.code !== "ESRCH") error = err;
        return false;
      }
    };
    const stop = () => {
      if (escalation || !send("SIGTERM")) return;
      cleanupDone = new Promise((done) => {
        escalation = setTimeout(() => {
          send("SIGKILL");
          done();
        }, PROCESS_STOP_GRACE_MS);
      });
    };
    const interrupt = (sig) => {
      interrupted = sig;
      stop();
    };
    const abort = () => interrupt("SIGTERM");
    const handlers = signal
      ? []
      : ["SIGINT", "SIGTERM", "SIGHUP"].map((sig) => {
          const handler = () => interrupt(sig);
          process.once(sig, handler);
          return [sig, handler];
        });
    const capture = (chunk) => {
      output += chunk.toString();
      appendFileSync(log, chunk);
      onOutput?.(chunk.toString());
    };
    child.stdout.on("data", capture);
    child.stderr.on("data", capture);
    child.on("error", (err) => {
      error = err;
    });
    child.once("exit", stop); // Also clean descendants after a failed or successful wrapper exit.
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    const deadline = setTimeout(() => {
      output += `\nCommand timed out after ${timeoutMs}ms\n`;
      appendFileSync(log, `\nCommand timed out after ${timeoutMs}ms\n`);
      interrupt("SIGTERM");
    }, timeoutMs);
    child.once("close", async (code, childSignal) => {
      clearTimeout(deadline);
      for (const [sig, handler] of handlers)
        process.removeListener(sig, handler);
      signal?.removeEventListener("abort", abort);
      await cleanupDone;
      if (error) {
        output += `\n${error.message}\n`;
        appendFileSync(log, `\n${error.message}\n`);
      }
      resolve({
        code: error ? 127 : code,
        signal: childSignal || interrupted,
        output,
      });
    });
  });
}
