#!/usr/bin/env node
import fs from "node:fs/promises";
import { constants } from "node:fs";
import path from "node:path";
import { execFile } from "node:child_process";
import { promisify, TextDecoder } from "node:util";
import { fileURLToPath } from "node:url";
import { recoverNativeSession } from "./native-session-recovery.mjs";

// Admission limits for the operator manifest and each inspector output, in bytes.
const JSON_LIMIT_BYTES = 2 * 1024 * 1024;
// Deadline of each fresh read-only inspector process, in milliseconds.
const INSPECT_TIMEOUT_MS = 15_000;
const executeFile = promisify(execFile);
const HELP = `Usage: node native-session-recovery-cli.mjs --accept-metadata-loss --request /abs/manifest.json --inspect /abs/readonly-inspector [args...]
Put all CLI options before --inspect. All subsequent arguments belong to the inspector.
The operator must select a trusted, read-only inspector. It runs without a shell or added privileges.
The inspector must independently verify current ownership, idle state, and unsent input evidence.
Historical Studio metadata and receipts remain unavailable. This operation never resumes or sends a model turn.
Only a recovery receipt reaches stdout. A retry refuses an existing descriptor.
After failure or a lost reply, inspect the descriptor's recovery requestId before any further action.
`;

function parse(argv) {
  let accepted = false,
    request,
    inspector;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--accept-metadata-loss" && !accepted) accepted = true;
    else if (argv[i] === "--request" && !request) request = argv[++i];
    else if (argv[i] === "--inspect") {
      inspector = argv.slice(i + 1);
      break;
    } else throw new Error("Invalid CLI options");
  }
  if (!accepted) throw new Error("Explicit metadata-loss consent is required");
  if (
    !request ||
    !path.isAbsolute(request) ||
    !inspector?.[0] ||
    !path.isAbsolute(inspector[0])
  )
    throw new Error("Absolute manifest and inspector paths are required");
  return { request, inspector };
}

function json(bytes) {
  if (
    !Buffer.isBuffer(bytes) ||
    bytes.length === 0 ||
    bytes.length > JSON_LIMIT_BYTES
  )
    throw new Error("Invalid JSON size");
  const value = JSON.parse(
    new TextDecoder("utf-8", { fatal: true }).decode(bytes),
  );
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("A JSON object is required");
  return value;
}

async function manifest(file) {
  const handle = await fs.open(
    file,
    constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
  );
  try {
    const stat = await handle.stat();
    if (!stat.isFile() || stat.size === 0 || stat.size > JSON_LIMIT_BYTES)
      throw new Error("Invalid manifest size");
    // A fixed allocation also bounds a file that grows after the stat.
    const buffer = Buffer.alloc(JSON_LIMIT_BYTES + 1);
    let size = 0;
    while (size < buffer.length) {
      const { bytesRead } = await handle.read(
        buffer,
        size,
        buffer.length - size,
        size,
      );
      if (!bytesRead) break;
      size += bytesRead;
    }
    return json(buffer.subarray(0, size));
  } finally {
    await handle.close();
  }
}

export async function main(
  argv,
  {
    stdout = process.stdout,
    stderr = process.stderr,
    execute = executeFile,
  } = {},
) {
  if (argv.length === 1 && argv[0] === "--help") {
    stdout.write(HELP);
    return 0;
  }
  if (!argv.includes("--accept-metadata-loss")) {
    stderr.write("Explicit metadata-loss consent is required. Use --help.\n");
    return 1;
  }
  try {
    const options = parse(argv);
    const request = await manifest(options.request);
    const receipt = await recoverNativeSession(request, {
      inspect: async () => {
        const { stdout: bytes } = await execute(
          options.inspector[0],
          options.inspector.slice(1),
          {
            timeout: INSPECT_TIMEOUT_MS,
            maxBuffer: JSON_LIMIT_BYTES,
            encoding: "buffer",
            shell: false,
            killSignal: "SIGKILL",
          },
        );
        return json(bytes);
      },
    });
    stdout.write(
      JSON.stringify({ type: "native-session-recovery", ...receipt }) + "\n",
    );
    return 0;
  } catch {
    // Child diagnostics, manifest content, and raw filesystem errors can contain secrets.
    stderr.write(
      "Native session recovery failed. Success is not confirmed. Inspect the descriptor before retrying.\n",
    );
    return 1;
  }
}

if (
  process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)
)
  process.exitCode = await main(process.argv.slice(2));
