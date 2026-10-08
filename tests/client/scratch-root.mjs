import {
  closeSync,
  fsyncSync,
  mkdtempSync,
  openSync,
  readFileSync,
  realpathSync,
  rmSync,
  statfsSync,
  writeSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { measuredFixtureScratchBytes } from "../../web/playwright-workers.mjs";

const maximumProbeBytes = 256 * 1024 * 1024;
const minimumHeadroomBytes = 256 * 1024 * 1024;

function memoryMounts() {
  try {
    return readFileSync("/proc/self/mountinfo", "utf8")
      .split("\n")
      .flatMap((line) => {
        const [before, after] = line.split(" - ");
        const fields = after?.split(" ");
        if (fields?.[0] !== "tmpfs") return [];
        const mountpoint = before?.split(" ")[4];
        if (!mountpoint) return [];
        return [
          mountpoint.replace(/\\([0-7]{3})/g, (_, code) =>
            String.fromCharCode(Number.parseInt(code, 8)),
          ),
        ];
      });
  } catch {
    return [];
  }
}

function probeCandidate(
  candidate,
  probeBytes,
  { requireMemoryMount = false } = {},
) {
  let probeDirectory;
  try {
    const parent = realpathSync(candidate);
    const mount = memoryMounts()
      .filter((point) => parent === point || parent.startsWith(`${point}/`))
      .sort((left, right) => right.length - left.length)[0];
    const memoryBacked = mount !== undefined;
    if (requireMemoryMount && !memoryBacked)
      throw new Error("not on a memory-backed filesystem");
    const fs = statfsSync(parent);
    const availableBytes = fs.bavail * fs.bsize;
    if (availableBytes < minimumHeadroomBytes + probeBytes)
      throw new Error(`only ${availableBytes} bytes of verified headroom`);

    probeDirectory = mkdtempSync(join(parent, ".codex-playwright-probe-"));
    const probePath = join(probeDirectory, "fsync-probe");
    const descriptor = openSync(probePath, "wx", 0o600);
    const block = Buffer.alloc(1024 * 1024, 0x5a);
    const started = performance.now();
    try {
      for (let remaining = probeBytes; remaining > 0; remaining -= block.length)
        writeSync(descriptor, block, 0, Math.min(block.length, remaining));
      fsyncSync(descriptor);
    } finally {
      closeSync(descriptor);
    }
    const writeFsyncMs = performance.now() - started;
    const verified = statfsSync(parent);
    return {
      parent,
      availableBytes: verified.bavail * verified.bsize,
      memoryBacked,
      mount,
      writeFsyncMs,
      passed: true,
    };
  } catch (error) {
    return {
      parent: candidate,
      passed: false,
      reason:
        error instanceof Error
          ? `${error.name}: ${error.message}`
          : String(error),
    };
  } finally {
    if (probeDirectory)
      rmSync(probeDirectory, { recursive: true, force: true });
  }
}

export function choosePlaywrightScratchRoot(
  environment = process.env,
  workerCount = Number(
    environment.CODEX_PLAYWRIGHT_SELECTED_WORKERS ??
      environment.PLAYWRIGHT_WORKERS ??
      1,
  ),
) {
  const probeBytes = Math.min(
    measuredFixtureScratchBytes * Math.max(1, workerCount),
    maximumProbeBytes,
  );
  const inheritedParent = environment.CODEX_PLAYWRIGHT_SCRATCH_PARENT;
  if (inheritedParent !== undefined) {
    const parent = realpathSync(inheritedParent);
    const stat = statfsSync(parent);
    const mount = memoryMounts()
      .filter((point) => parent === point || parent.startsWith(`${point}/`))
      .sort((left, right) => right.length - left.length)[0];
    const inherited = {
      parent,
      availableBytes: stat.bavail * stat.bsize,
      memoryBacked: mount !== undefined,
      mount,
      writeFsyncMs: 0,
      passed: true,
      inheritedProbe: true,
    };
    return createScratchRoot(inherited, [inherited], probeBytes, {
      root: parent,
      ownedRoot: false,
    });
  }

  const override = environment.PLAYWRIGHT_SCRATCH_ROOT;
  if (override !== undefined) {
    const result = probeCandidate(resolve(override), probeBytes);
    if (!result.passed)
      throw new Error(
        `PLAYWRIGHT_SCRATCH_ROOT probe failed for ${resolve(override)}: ${result.reason}`,
      );
    return createScratchRoot(result, [result], probeBytes, {
      root: result.parent,
      ownedRoot: false,
    });
  }

  const mounts = memoryMounts();
  const candidates = [...mounts, "/tmp", environment.TMPDIR, tmpdir()].filter(
    (value, index, values) => value && values.indexOf(value) === index,
  );
  const tested = candidates.map((candidate) =>
    probeCandidate(candidate, probeBytes),
  );
  const eligible = tested
    .filter((result) => result.passed && result.memoryBacked)
    .sort((left, right) => right.availableBytes - left.availableBytes);
  const chosen = eligible[0] ?? tested.find((result) => result.passed);
  if (!chosen)
    throw new Error(
      `No temporary filesystem passed the ${probeBytes}-byte fsync probe: ${JSON.stringify(tested)}`,
    );
  return createScratchRoot(chosen, tested, probeBytes);
}

function createScratchRoot(
  chosen,
  tested = [chosen],
  probeBytes,
  options = {},
) {
  const root = options.root ?? mkdtempSync(join(chosen.parent, "pw-"));
  return Object.freeze({
    root,
    ownedRoot: options.ownedRoot ?? true,
    memoryBacked: chosen.memoryBacked,
    mount: chosen.mount,
    availableBytes: chosen.availableBytes,
    probeBytes,
    writeFsyncMs: chosen.writeFsyncMs,
    inheritedProbe: chosen.inheritedProbe ?? false,
    tested,
  });
}
