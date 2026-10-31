// Host-side installer fixture. Production uses the guest's existing Python runtime.
import assert from "node:assert/strict";
import { execFile, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statfsSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { TestContext } from "node:test";

const guest = path.dirname(fileURLToPath(import.meta.url));
export const binary = { format: 1, version: "0.5.1", build: "fixture" };
export const compatible = {
  binary,
  store: binary,
  compatible: true,
  state: "compatible",
  upgrade: null,
};
export const legacy = {
  binary,
  store: null,
  compatible: false,
  state: "legacy",
  upgrade: { fromFormat: 0, toFormat: 1, metadataOnly: true },
};

export function fixture(t: TestContext, report: unknown = legacy) {
  const temporary =
    process.platform === "darwin" ? "/private/tmp" : os.tmpdir();
  const disk = statfsSync(temporary);
  assert(
    disk.bavail * disk.bsize >= 15 * 1024 ** 3,
    "Fixtures need 15 GiB free",
  );
  const root = mkdtempSync(path.join(temporary, "vm-upgrade-"));
  const file = (name: string) => path.join(root, name.replace(/^\//, ""));
  const write = (name: string, body: string, mode = 0o644) => {
    mkdirSync(path.dirname(file(name)), { recursive: true });
    writeFileSync(file(name), body, { mode });
  };
  const state = "/var/lib/codex-studio";
  const store = `${state}/layr`;
  write(`${store}/report.json`, JSON.stringify(report));
  write(`${store}/after.json`, JSON.stringify(compatible));
  write(`${store}/machine-key`, "keep-key");
  write(`${store}/project-data`, "keep-project");
  write("/usr/local/bin/layr", "#!/bin/sh\necho old-binary\n", 0o755);
  write(
    "/candidate",
    `#!/bin/sh
case "$*" in
  *--json) cat "$LAYR_ROOT/report.json" ;;
  *--upgrade)
    test -e '${file("stopped")}' || { echo 'upgrade before stop' >&2; exit 1; }
    printf 'upgrade\\n' >> "$LAYR_ROOT/calls"
    if [ -e "$LAYR_ROOT/fail" ]; then echo 'upgrade failed' >&2; exit 1; fi
    cp "$LAYR_ROOT/after.json" "$LAYR_ROOT/report.json"
    if [ -e "$LAYR_ROOT/lost" ]; then echo 'lost response' >&2; exit 1; fi ;;
esac
`,
    0o755,
  );
  const identities: string[] = [];
  for (const name of ["layr", "layr-desk"]) {
    const source = `/opt/codex-studio/vm/${name}`;
    const cargo = "[package]\n";
    const files = {
      "Cargo.toml": createHash("sha256").update(cargo).digest("hex"),
    };
    const identity = createHash("sha256")
      .update(JSON.stringify(files))
      .digest("hex");
    identities.push(identity);
    write(`${source}/Cargo.toml`, cargo);
    write(
      `${source}/manifest.json`,
      JSON.stringify({
        files,
        sourceSha256: identity,
        revision: "a".repeat(40),
      }),
    );
  }
  for (const name of ["layr", "layr-admin", "share", "share-bridge"])
    write(
      `/opt/codex-studio/vm/guest/codex-studio-${name}.service`,
      "[Unit]\n",
    );
  for (const name of [
    "guest_update.py",
    "common.py",
    "shared_values_generated.py",
  ])
    write(
      `/opt/codex-studio/vm/guest/${name}`,
      readFileSync(path.join(guest, name), "utf8"),
    );
  for (const [name, body] of Object.entries({
    id: "echo 0",
    findmnt: "echo btrfs",
    node: "exit 99",
    python3: `exec '${execFileSync("which", [process.env.STUDIO_GUEST_PYTHON || "python3"], { encoding: "utf8" }).trim()}' "$@"`,
    flock: process.platform === "linux" ? 'exec /usr/bin/flock "$@"' : ":",
    systemctl: `echo "$*" >> '${file("systemctl.log")}'
case "$*" in
stop*) touch '${file("stopped")}' ;;
start*) if [ -f '${file("fail-start")}' ]; then rm '${file("fail-start")}'; echo 'start failed' >&2; exit 1; fi ;;
esac`,
    timeout: 'shift 2; exec "$@"',
  }))
    write(`/bin/${name}`, `#!/bin/sh\n${body}\n`, 0o755);
  write(
    "/opt/codex-studio/rust/bin/cargo",
    `#!/bin/sh
printf '%s\\n' "\${LAYR_BUILD_ID:-}" >> '${file("build-environments")}'
mkdir -p "$CARGO_TARGET_DIR/release"
case "$*" in *layr-desk*) echo desk > "$CARGO_TARGET_DIR/release/layr-desk" ;;
*) cp '${file("candidate")}' "$CARGO_TARGET_DIR/release/layr" ;; esac
`,
    0o755,
  );
  mkdirSync(file("/etc/systemd/system"), { recursive: true });
  let script = readFileSync(path.join(guest, "install-layr.sh"), "utf8");
  for (const prefix of [
    "/opt/codex-studio",
    "/var/lib/codex-studio",
    "/etc/systemd/system",
    "/usr/local/bin",
  ])
    script = script.replaceAll(prefix, file(prefix));
  write("/install.sh", script);
  const socketPath = file("hello.sock");
  const server = createServer((socket) => {
    let request = Buffer.alloc(0);
    socket.on("data", (chunk: Buffer) => {
      request = Buffer.concat([request, chunk]);
      if (request.length < 4 || request.length < request.readUInt32BE(0) + 4)
        return;
      assert.deepEqual(JSON.parse(request.subarray(4).toString()), {
        hello: true,
      });
      const body = Buffer.from(
        JSON.stringify({
          ...binary,
          format: existsSync(file("bad-hello")) ? 9 : 1,
        }),
      );
      const header = Buffer.alloc(5);
      header[0] = 86;
      header.writeUInt32BE(body.length, 1);
      socket.end(Buffer.concat([header, body]));
    });
  });
  const listening = new Promise<void>((resolve) =>
    server.listen(socketPath, resolve),
  );
  t.after(async () => {
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
    rmSync(root, { recursive: true, force: true });
  });
  const run = async ({
    defer = false,
    activate = false,
    status = "-",
    units = [],
  }: {
    defer?: boolean;
    activate?: boolean;
    status?: string;
    units?: string[];
  } = {}) => {
    await listening;
    return await new Promise<{
      status: number;
      stdout: string;
      stderr: string;
    }>((resolve) => {
      execFile(
        "bash",
        [
          file("install.sh"),
          ...(activate ? ["--activate", status, ...units] : []),
        ],
        {
          encoding: "utf8",
          timeout: 20_000,
          env: {
            ...process.env,
            LAYR_SOCKET: socketPath,
            STUDIO_DEFER_RESTARTS: defer ? "1" : "",
            PATH: `${file("bin")}:${process.env.PATH}`,
          },
        },
        (error, stdout, stderr) =>
          resolve({
            status:
              error === null
                ? 0
                : typeof error.code === "number"
                  ? error.code
                  : 1,
            stdout,
            stderr,
          }),
      );
    });
  };
  const read = (name: string) => readFileSync(file(name), "utf8");
  return {
    root,
    file,
    write,
    read,
    run,
    state,
    store,
    identity: identities[0]!,
  };
}
