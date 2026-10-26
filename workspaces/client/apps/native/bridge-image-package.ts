import fs from "node:fs/promises";
import path from "node:path";

// The bridge manifest owns the native package set. A deploy keeps every required
// platform; the native app retains only the codecs its host can execute.
export async function pruneBridgeImageCodecs(
  directory: string,
  { platform = process.platform, arch = process.arch } = {},
) {
  const manifest: unknown = JSON.parse(
    await fs.readFile(path.join(directory, "package.json"), "utf8"),
  );
  if (
    !manifest ||
    typeof manifest !== "object" ||
    !("dependencies" in manifest) ||
    !manifest.dependencies ||
    typeof manifest.dependencies !== "object"
  )
    throw new Error("The bridge manifest must provide its dependency map.");
  const names = Object.keys(manifest.dependencies).filter((name) =>
    /^@img\/sharp-(?:libvips-)?(?:darwin|linux)-/.test(name),
  );
  const keep = [
    `@img/sharp-${platform}-${arch}`,
    `@img/sharp-libvips-${platform}-${arch}`,
  ];
  for (const name of keep) {
    if (!names.includes(name))
      throw new Error(`The bridge has no image codec for ${platform}-${arch}.`);
    await fs.access(path.join(directory, "node_modules", name));
  }
  const removed = names.filter((name) => !keep.includes(name));
  // Remove native bindings before the libraries that they reference.
  for (const name of removed.filter((name) => !name.includes("libvips")))
    await fs.rm(path.join(directory, "node_modules", name), {
      recursive: true,
      force: true,
    });
  for (const name of removed.filter((name) => name.includes("libvips")))
    await fs.rm(path.join(directory, "node_modules", name), {
      recursive: true,
      force: true,
    });
  return { platform, arch, kept: keep, removed };
}
