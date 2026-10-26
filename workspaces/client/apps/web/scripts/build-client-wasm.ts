import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
import { homedir } from "node:os";
const root = fileURLToPath(new URL("../../../../../", import.meta.url));
function run(command: string, args: string[]) {
  const result = spawnSync(command, args, { cwd: root, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status || 1);
}
// The parser crate owns the binding ABI version; the CLI must match it exactly.
const manifest = readFileSync(
  resolve(root, "workspaces/client/packages/client-wasm/Cargo.toml"),
  "utf8",
);
const bindingVersion = manifest.match(
  /^wasm-bindgen\s*=\s*"=([^"\s]+)"\s*$/m,
)?.[1];
if (!bindingVersion)
  throw new Error("client-wasm must pin one exact wasm-bindgen version.");
const executable =
  process.platform === "win32" ? "wasm-bindgen.exe" : "wasm-bindgen";
// Accept only the crate's exact ABI, whether installed by Cargo or in Studio's
// versioned UI tool cache. A different CLI on PATH never substitutes for it.
const candidates = [
  executable,
  resolve(
    process.env.CARGO_HOME || resolve(homedir(), ".cargo"),
    "bin",
    executable,
  ),
  resolve(
    process.env.XDG_CACHE_HOME || resolve(homedir(), ".cache"),
    "codex-studio-rust-ui",
    `wasm-bindgen-${bindingVersion}`,
    "bin",
    executable,
  ),
];
const bindingCli = candidates.find((candidate) => {
  const version = spawnSync(candidate, ["--version"], { encoding: "utf8" });
  return (
    version.status === 0 &&
    version.stdout?.trim() === `wasm-bindgen ${bindingVersion}`
  );
});
if (!bindingCli) {
  throw new Error(
    `Install the matching binding generator: cargo install wasm-bindgen-cli --version ${bindingVersion} --locked`,
  );
}
run("cargo", [
  "build",
  "-p",
  "studio-client-wasm",
  "--release",
  "--target",
  "wasm32-unknown-unknown",
  "--locked",
]);
const target = process.env.CARGO_TARGET_DIR
  ? resolve(root, process.env.CARGO_TARGET_DIR)
  : resolve(root, "target");
run(bindingCli, [
  resolve(target, "wasm32-unknown-unknown/release/studio_client_wasm.wasm"),
  "--target",
  "web",
  "--out-dir",
  "workspaces/client/apps/web/src/wasm/generated",
  "--out-name",
  "client",
]);
