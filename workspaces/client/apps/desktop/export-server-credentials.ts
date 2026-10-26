import type { SafeStorage } from "electron";
import { readFile } from "node:fs/promises";
import { spawn } from "node:child_process";
import path from "node:path";

// Secrets travel only through a pipe to the native key-store importer. Never
// include child output or decrypted data in an error or profile archive.
export async function exportServerCredentials({
  profile,
  destination,
  safeStorage,
}: {
  profile: string;
  destination?: unknown;
  safeStorage: Pick<SafeStorage, "isEncryptionAvailable" | "decryptString"> &
    Partial<Pick<SafeStorage, "getSelectedStorageBackend">>;
}) {
  let encrypted;
  try {
    encrypted = await readFile(
      path.join(profile, "paired-servers.json"),
      "utf8",
    );
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT")
      return;
    throw new Error("Could not read the saved server keys.");
  }
  if (
    !destination ||
    typeof destination !== "object" ||
    !("executable" in destination) ||
    typeof destination.executable !== "string" ||
    !("profile" in destination) ||
    typeof destination.profile !== "string" ||
    !path.isAbsolute(destination.executable ?? "") ||
    !path.isAbsolute(destination.profile)
  )
    throw new Error(
      "Saved server keys require a native credential migration destination.",
    );
  if (
    !safeStorage.isEncryptionAvailable() ||
    safeStorage.getSelectedStorageBackend?.() === "basic_text"
  )
    throw new Error("Secure key storage is unavailable.");
  let bytes;
  try {
    bytes = Buffer.from(
      safeStorage.decryptString(Buffer.from(encrypted, "base64")),
    );
    JSON.parse(bytes.toString());
  } catch {
    throw new Error("Could not decrypt the saved server keys.");
  }
  const { executable, profile: destinationProfile } = destination;
  try {
    if (bytes.length > 1024 * 1024)
      throw new Error("Saved server keys exceed the migration limit.");
    await new Promise<void>((resolve, reject) => {
      const child = spawn(
        executable,
        ["--import-server-credentials", destinationProfile],
        { stdio: ["pipe", "ignore", "ignore"], timeout: 60000 },
      );
      // An early exit closes stdin; the process result determines success.
      child.stdin.on("error", () => {});
      child.once("error", () =>
        reject(new Error("Could not start the native key importer.")),
      );
      child.once("close", (code) =>
        code === 0
          ? resolve()
          : reject(
              new Error(
                "Native key import failed. The original keys are unchanged.",
              ),
            ),
      );
      child.stdin.end(bytes);
    });
  } finally {
    bytes.fill(0);
  }
}
