// Rust executables shipped in workspace/bin by both desktop packagers.
export const runtimeBinaries = Object.freeze([
  { package: "studio-runtime-server", binary: "studio-runtime-server" },
  { package: "studio-process-supervisor", binary: "codex-process-supervisor" },
]);

export function runtimeBinaryName(binary: string, platform = process.platform) {
  return platform === "win32" ? `${binary}.exe` : binary;
}
