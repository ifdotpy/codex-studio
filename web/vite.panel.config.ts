import { defineConfig } from "vite";
import { z } from "zod";
import { definitions, panelCatalog, panelContract } from "./src/panel/catalog";

export default defineConfig({
  publicDir: false,
  plugins: [
    {
      name: "studio-panel-catalog",
      generateBundle() {
        const components = Object.fromEntries(
          Object.entries(definitions).map(([name, definition]) => [
            name,
            {
              description: definition.description,
              props: z.toJSONSchema(definition.props),
              children: "slots" in definition,
            },
          ]),
        );
        this.emitFile({
          type: "asset",
          fileName: "panel-catalog.json",
          source:
            JSON.stringify(
              {
                ...panelContract,
                components,
                specSchema: panelCatalog.jsonSchema(),
              },
              null,
              2,
            ) + "\n",
        });
      },
    },
  ],
  build: {
    emptyOutDir: false,
    lib: {
      entry: "src/panel/entry.tsx",
      name: "StudioPanel",
      formats: ["iife"],
      fileName: () => "assets/panel-ui.js",
    },
  },
  define: { "process.env.NODE_ENV": JSON.stringify("production") },
});
