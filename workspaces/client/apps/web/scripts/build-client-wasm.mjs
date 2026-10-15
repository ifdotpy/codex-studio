import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
const root = fileURLToPath(new URL("../../../../../", import.meta.url));
function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status || 1);
}
const version = spawnSync("wasm-bindgen", ["--version"], { encoding: "utf8" });
if (version.stdout?.trim() !== "wasm-bindgen 0.2.129") {
  throw new Error(
    "Install the matching binding generator: cargo install wasm-bindgen-cli --version 0.2.129 --locked",
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
run("wasm-bindgen", [
  resolve(target, "wasm32-unknown-unknown/release/studio_client_wasm.wasm"),
  "--target",
  "web",
  "--out-dir",
  "workspaces/client/apps/web/src/wasm/generated",
  "--out-name",
  "client",
]);
