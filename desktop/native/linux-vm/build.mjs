import { execFileSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import path from "node:path";
import { signLinuxVM, signingIdentity } from "../../signing.mjs";

const source = path.dirname(fileURLToPath(import.meta.url));
export function buildLinuxVM(output, identity = signingIdentity()) {
  execFileSync(
    "xcrun",
    [
      "swiftc",
      path.join(source, "main.swift"),
      "-O",
      "-target",
      "arm64-apple-macos13.0",
      "-framework",
      "Virtualization",
      "-o",
      output,
    ],
    { stdio: "inherit", timeout: 120_000 },
  );
  signLinuxVM(output, identity);
  execFileSync(output, ["--check"], { stdio: "inherit", timeout: 10_000 });
  return output;
}
if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href
) {
  buildLinuxVM(
    path.resolve(process.argv[2] || path.join(source, "studio-linux-vm")),
  );
}
