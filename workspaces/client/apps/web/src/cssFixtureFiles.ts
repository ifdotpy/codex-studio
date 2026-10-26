import { readdirSync } from "node:fs";
import { join } from "node:path";

/** Shared source discovery for the CSS contract witnesses. */
export function cssFiles(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory()
      ? cssFiles(path)
      : entry.name.endsWith(".css")
        ? [path]
        : [];
  });
}
