import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import { nativeImportGraphPlugin } from "./scripts/nativeImportGraph.mjs";
import { nativePlatformAliases } from "./scripts/nativePlatformAliases.mjs";
import { nativeUiAdapters } from "./scripts/nativeUiAdapters.mjs";

const graph = nativeImportGraphPlugin({
  entry: "src/native/main.tsx",
  uiAdapters: nativeUiAdapters,
});

export default defineConfig({
  base: "./",
  publicDir: false,
  resolve: { alias: nativePlatformAliases },
  plugins: [graph.plugin],
  build: {
    outDir: "dist-native",
    rolldownOptions: {
      input: fileURLToPath(new URL("./native.html", import.meta.url)),
    },
  },
});
