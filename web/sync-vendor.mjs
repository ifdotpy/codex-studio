// Keep the local browser bundle reproducible from package-lock.json.
import { copyFile, mkdir } from "node:fs/promises";
await mkdir(new URL("./vendor/", import.meta.url), { recursive: true });
for (const [source, target] of [
  ["marked/lib/marked.umd.js", "marked.js"],
  ["marked/LICENSE", "marked.LICENSE"],
  ["dompurify/dist/purify.min.js", "purify.js"],
  ["dompurify/LICENSE", "purify.LICENSE"],
]) {
  await copyFile(
    new URL("./node_modules/" + source, import.meta.url),
    new URL("./vendor/" + target, import.meta.url),
  );
}
