import { mkdtemp, mkdir, writeFile, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, expect, it } from "vitest";
import { build } from "vite";
import { createHash } from "node:crypto";
import {
  checkNativeImportGraph,
  nativeImportGraphPlugin,
  buildNativeRenderer,
  verifyNativeArtifactAudit,
  NATIVE_AUDIT_FILENAME,
} from "../scripts/nativeImportGraph.mjs";

const roots = [];
afterEach(async () => {
  await Promise.all(
    roots.splice(0).map((root) => rm(root, { recursive: true, force: true })),
  );
});

async function fixture(files) {
  const root = await mkdtemp(join(tmpdir(), "studio-native-graph-"));
  roots.push(root);
  for (const [name, source] of Object.entries(files)) {
    const path = join(root, name);
    await mkdir(join(path, ".."), { recursive: true });
    await writeFile(path, source);
  }
  return root;
}

async function emittedFixture(root, extraPlugins = [], options = {}) {
  const entry = join(root, "native.ts");
  const checker = nativeImportGraphPlugin({ entry, ...options });
  const output = await build({
    root,
    configFile: false,
    logLevel: "silent",
    plugins: [checker.plugin, ...extraPlugins],
    build: {
      write: false,
      minify: false,
      sourcemap: true,
      rolldownOptions: { input: entry },
    },
  });
  await checker.finalize?.(output);
  return checker.report();
}

it("proves erased vendor barrel I/O has no emitted code while retaining its source hash", async () => {
  const root = await fixture({
    "native.ts": 'import { view } from "view-kit"; console.log(view);',
    "node_modules/view-kit/package.json": JSON.stringify({
      name: "view-kit",
      type: "module",
      exports: "./index.js",
      sideEffects: false,
    }),
    "node_modules/view-kit/index.js":
      'export { view } from "./view.js"; export { load } from "./io.js";',
    "node_modules/view-kit/view.js": 'export const view = "UI";',
    "node_modules/view-kit/io.js": 'export const load = () => fetch("/api");',
  });
  const report = await emittedFixture(root);
  expect(report.findings).toEqual([]);
  const erased = report.inventory.modules.find((module) =>
    module.id.endsWith("view-kit/io.js"),
  );
  expect(erased).toMatchObject({ disposition: "omitted", renderedBytes: 0 });
  expect(erased.sourceHash).toMatch(/^[a-f0-9]{64}$/);
  expect(erased.emissions).toEqual([]);
});

it.each([
  ['fetch("/api/private");', "browser-network"],
  ["navigator.clipboard.readText();", "browser-clipboard"],
  ['localStorage.setItem("account", "changed");', "browser-persistence"],
  ['new Blob(["bytes"]);', "browser-bytes"],
])(
  "rejects capability code added after source parsing: %s",
  async (source, capability) => {
    const root = await fixture({ "native.ts": 'console.log("view");' });
    await expect(
      emittedFixture(root, [
        {
          name: "late-output-witness",
          generateBundle: {
            order: "post",
            handler(_options, bundle) {
              for (const item of Object.values(bundle))
                if (item.type === "chunk") item.code += "\n" + source;
            },
          },
        },
      ]),
    ).rejects.toThrow(capability);
  },
);

it("inspects executable assets emitted outside the module graph", async () => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  await expect(
    emittedFixture(root, [
      {
        name: "executable-asset-witness",
        generateBundle() {
          this.emitFile({
            type: "asset",
            fileName: "worker.js",
            source: 'fetch("/api");',
          });
        },
      },
    ]),
  ).rejects.toThrow("browser-network");
});

it("rejects retained vendor I/O even when its rendered-length metadata is falsified", async () => {
  const root = await fixture({
    "native.ts": 'import { load } from "view-kit"; load();',
    "node_modules/view-kit/package.json": JSON.stringify({
      name: "view-kit",
      type: "module",
      exports: "./index.js",
    }),
    "node_modules/view-kit/index.js":
      'export const load = () => fetch("/api");',
  });
  await expect(
    emittedFixture(root, [
      {
        name: "false-emission-length-witness",
        generateBundle(_options, bundle) {
          for (const item of Object.values(bundle))
            if (item.type === "chunk") {
              for (const [id, metadata] of Object.entries(item.modules))
                if (id.includes("view-kit")) {
                  metadata.renderedLength = 0;
                  metadata.code = null;
                }
            }
        },
      },
    ]),
  ).rejects.toThrow(/browser-network|emission/);
});

it("rejects a browser owner dynamically imported behind hasNativeClient", async () => {
  const root = await fixture({
    "native.ts":
      'import { hasNativeClient } from "./platform"; if (!hasNativeClient()) void import("./browser");',
    "platform.ts":
      "export function hasNativeClient() { return Boolean(globalThis.__TAURI__); }",
    "browser.ts": 'export const saved = localStorage.getItem("pending");',
  });
  await expect(
    checkNativeImportGraph({ root, entry: join(root, "native.ts") }),
  ).rejects.toThrow(/native\.ts.*browser\.ts.*browser-persistence/s);
});

it("checks resolved virtual modules after build-plugin transforms", async () => {
  const root = await fixture({ "native.ts": 'import "virtual:adapter";' });
  const entry = join(root, "native.ts");
  const checker = nativeImportGraphPlugin({ entry });
  await expect(
    build({
      root,
      configFile: false,
      logLevel: "silent",
      plugins: [
        {
          name: "fixture-adapter",
          resolveId(id) {
            if (id === "virtual:adapter") return "\0virtual:adapter";
          },
          load(id) {
            if (id === "\0virtual:adapter")
              return 'fetch("/api/write", {method:"POST"});';
          },
        },
        checker.plugin,
      ],
      build: {
        write: false,
        minify: false,
        rolldownOptions: { input: entry, treeshake: false },
      },
    }),
  ).rejects.toThrow(/virtual:adapter.*browser-network/s);
});

it("resolves lexical bindings in virtual modules", async () => {
  const root = await fixture({ "native.ts": 'import "virtual:adapter";' });
  const entry = join(root, "native.ts");
  const checker = nativeImportGraphPlugin({ entry });
  await build({
    root,
    configFile: false,
    logLevel: "silent",
    plugins: [
      {
        name: "fixture-adapter",
        resolveId(id) {
          if (id === "virtual:adapter") return "\0virtual:adapter";
        },
        load(id) {
          if (id === "\0virtual:adapter")
            return 'export function send(fetch) { return fetch("native"); }';
        },
      },
      checker.plugin,
    ],
    build: {
      write: false,
      rolldownOptions: { input: entry, treeshake: false },
    },
  });
  expect(checker.report().findings).toEqual([]);
});

it("follows aliases and static re-exports with an import trace", async () => {
  const root = await fixture({
    "native.ts": 'import { send } from "@services"; console.log(send);',
    "services.ts": 'export { send } from "./transport";',
    "transport.ts": 'export const send = () => window["fetch"]("/api/state");',
  });
  await expect(
    checkNativeImportGraph({
      root,
      entry: join(root, "native.ts"),
      resolve: {
        alias: { "@services": join(root, "services.ts") },
      },
    }),
  ).rejects.toThrow(
    /native\.ts -> services\.ts -> transport\.ts.*browser-network/s,
  );
});

it("uses Vite browser package exports rather than a separate resolver", async () => {
  const root = await fixture({
    "native.ts": 'import "platform-adapter";',
    "node_modules/platform-adapter/package.json": JSON.stringify({
      name: "platform-adapter",
      type: "module",
      exports: { browser: "./browser.js", default: "./native.js" },
    }),
    "node_modules/platform-adapter/browser.js":
      "export const saved = indexedDB.databases();",
    "node_modules/platform-adapter/native.js": "export const saved = [];",
  });
  await expect(
    checkNativeImportGraph({ root, entry: join(root, "native.ts") }),
  ).rejects.toThrow(/platform-adapter\/browser\.js.*browser-persistence/s);
});

it("rejects browser persistence packages even when their fixture code is pure", async () => {
  const root = await fixture({
    "native.ts": 'import { version } from "rxdb"; console.log(version);',
    "node_modules/rxdb/package.json": JSON.stringify({
      name: "rxdb",
      type: "module",
      exports: "./index.js",
    }),
    "node_modules/rxdb/index.js": 'export const version = "fixture";',
  });
  await expect(
    checkNativeImportGraph({ root, entry: join(root, "native.ts") }),
  ).rejects.toThrow(/rxdb\/index\.js.*browser-owner/s);
});

it("checks project sources before production tree shaking removes unused imports", async () => {
  const root = await fixture({
    "native.ts": 'import "./browser"; console.log("UI");',
    "browser.ts":
      'export const unused = () => localStorage.getItem("pending");',
  });
  await expect(
    checkNativeImportGraph({ root, entry: join(root, "native.ts") }),
  ).rejects.toThrow("browser-persistence");
});

it("permits resolved function type checks and static prototype introspection", async () => {
  const root = await fixture({
    "native.ts":
      'import { describe } from "./presentation"; console.log(describe(() => 1));',
    "presentation.ts": `
      const prototype = Function.prototype;
      const toText = prototype.toString;
      const bind = Function.prototype.bind;
      export function describe(value) {
        return [value instanceof Function, toText.call(value), bind.call(value, null)];
      }
    `,
  });
  expect(
    (await checkNativeImportGraph({ root, entry: join(root, "native.ts") }))
      .findings,
  ).toEqual([]);
});

it("permits the actual DOMPurify dependency's function type checks", async () => {
  const dependency = fileURLToPath(
    new URL("../node_modules/dompurify/dist/purify.es.mjs", import.meta.url),
  );
  const root = await fixture({
    "native.ts": `export { default as sanitize } from ${JSON.stringify(dependency)};`,
  });
  const report = await checkNativeImportGraph({
    root,
    entry: join(root, "native.ts"),
  });
  expect(
    report.modules.some((id) => id.endsWith("dompurify/dist/purify.es.mjs")),
  ).toBe(true);
  expect(report.findings).toEqual([]);
});

it.each([
  'Function("return 1");',
  'new Function("return 1");',
  'const Constructor = Function; new Constructor("return 1");',
  'const { Function: Constructor } = window; Constructor("return 1");',
  'const host = globalThis; host["Function"]("return 1");',
  'Function.prototype.constructor("return 1");',
  'const prototype = Function.prototype; const Constructor = prototype.constructor; Constructor("return 1");',
  'const prototype = Function.prototype; prototype[externalName]("return 1");',
  "const prototype = Function.prototype; consume(prototype);",
  "const prototype = Function.prototype; export { prototype };",
  'const method = Function.prototype.toString; method.constructor("return 1");',
  'const method = Function.prototype.toString; method[externalName]("return 1");',
  'Function.prototype.bind(null).constructor("return 1");',
  'const bound = Function.prototype.bind(null); const Constructor = bound.constructor; Constructor("return 1");',
  "Function.prototype.toString = foreignMethod;",
  'eval("1");',
  'const execute = eval; execute("1");',
])(
  "preserves runtime code and prototype escape rejection: %s",
  async (source) => {
    const root = await fixture({
      "native.ts": 'import "./runtime";',
      "runtime.ts": source,
    });
    await expect(
      checkNativeImportGraph({ root, entry: join(root, "native.ts") }),
    ).rejects.toThrow(/runtime-code/);
  },
);

it.each([
  ['crypto.subtle.digest("SHA-256", new Uint8Array());', "browser-crypto"],
  ["const host = globalThis; host.crypto.randomUUID();", "browser-crypto"],
  ['const {fetch:send} = window; send("/api");', "browser-network"],
  ['const name = "fetch"; window[name]("/api");', "dynamic-browser-global"],
  [
    'function send(host) { host.fetch("/api"); } send(window);',
    "browser-global-escape",
  ],
  ["export function browser() { return window; }", "browser-global-escape"],
  ["export const bridge = { host: window };", "browser-global-escape"],
  [
    'const {navigator:host} = globalThis; host.sendBeacon("/api", "body");',
    "browser-network",
  ],
  ['new EventSource("/api/events");', "browser-network"],
  ['navigator.sendBeacon("/api", "body");', "browser-network"],
  [
    'const {sendBeacon:send} = navigator; send("/api", "body");',
    "browser-network",
  ],
  ['window.localStorage.getItem("saved");', "browser-persistence"],
  ['globalThis.localStorage.getItem("saved");', "browser-persistence"],
  ['document.cookie = "saved=1";', "browser-persistence"],
  ["setInterval(() => {}, 1000);", "business-timer"],
  ['new Worker("./worker.js");', "browser-worker"],
  [
    "const target = globalThis.target; void import(/* @vite-ignore */ target);",
    "unresolved-dynamic-import",
  ],
])("rejects browser authority: %s", async (source, capability) => {
  const root = await fixture({ "native.ts": source });
  await expect(
    checkNativeImportGraph({ root, entry: join(root, "native.ts") }),
  ).rejects.toThrow(capability);
});

it.each([
  ["new FileReader();", "browser-bytes"],
  ["new FileReaderSync();", "browser-bytes"],
  ['new Blob(["bytes"]);', "browser-bytes"],
  ['new File(["bytes"], "message.txt");', "browser-bytes"],
  ["const host = window; new host.FileReader();", "browser-bytes"],
  ['const {Blob:Bytes} = globalThis; new Bytes(["bytes"]);', "browser-bytes"],
  ["new MediaRecorder(stream);", "browser-audio"],
  ["new AudioContext();", "browser-audio"],
  ["new OfflineAudioContext(1, 1024, 48000);", "browser-audio"],
  ["new window.webkitAudioContext();", "browser-audio"],
  [
    "const {MediaRecorder:Recorder} = self; new Recorder(stream);",
    "browser-audio",
  ],
  ["navigator.mediaDevices.getUserMedia({ audio: true });", "browser-device"],
  [
    "const {mediaDevices:capture} = navigator; capture.getUserMedia({audio:true});",
    "browser-device",
  ],
  [
    'const {navigator:host} = window; host["clipboard"].readText();',
    "browser-clipboard",
  ],
  [
    'const host = navigator; const {clipboard:port} = host; port.writeText("code");',
    "browser-clipboard",
  ],
  [
    'const {["clipboard"]:port} = navigator; port.readText();',
    "browser-clipboard",
  ],
  ["URL.createObjectURL(bytes);", "browser-bytes"],
  ['window.URL["revokeObjectURL"]("blob:fixture");', "browser-bytes"],
  [
    "const {URL:Urls} = globalThis; const urls = Urls; urls.createObjectURL(bytes);",
    "browser-bytes",
  ],
  ["const {createObjectURL:create} = URL; create(bytes);", "browser-bytes"],
  ['const {["createObjectURL"]:create} = URL; create(bytes);', "browser-bytes"],
  [
    "const member = externalName; const {[member]:port} = navigator; port();",
    "dynamic-browser-global",
  ],
  ['new Notification("message");', "browser-notification"],
  [
    'const {Notification:Notice} = window; new Notice("message");',
    "browser-notification",
  ],
  ['window.open("https://example.test");', "browser-window"],
  [
    'const {open:launch} = self; launch("https://example.test");',
    "browser-window",
  ],
  ...[
    "geolocation",
    "bluetooth",
    "usb",
    "serial",
    "hid",
    "requestMIDIAccess",
    "getUserMedia",
  ].map((member) => [`navigator["${member}"];`, "browser-device"]),
])("rejects resolved browser I/O ownership: %s", async (source, capability) => {
  const root = await fixture({
    "native.ts": 'import "./device";',
    "device.ts": source,
  });
  await expect(
    checkNativeImportGraph({ root, entry: join(root, "native.ts") }),
  ).rejects.toThrow(
    new RegExp(`native\\.ts -> device\\.ts.*${capability}`, "s"),
  );
});

it("permits pure URL parsing, DOM input metadata, events, and injected byte or audio names", async () => {
  const root = await fixture({
    "native.ts": `
      import { File, Blob } from "./icons";
      const page = new URL("/view?tab=chat", "https://example.test");
      URL.canParse(page.href);
      const input = document.createElement("input");
      input.addEventListener("change", (event) => {
        document.body.textContent = event.target.files?.[0]?.name || input.value;
      });
      export function render(AudioContext, MediaRecorder, navigator) {
        return { File, Blob, AudioContext, MediaRecorder, clipboard: navigator.clipboard };
      }
    `,
    "icons.ts":
      'export const File = "file icon"; export const Blob = "blob icon";',
  });
  expect(
    (await checkNativeImportGraph({ root, entry: join(root, "native.ts") }))
      .findings,
  ).toEqual([]);
});

it("does not extend an exact UI timer exception to browser device or byte authority", async () => {
  const root = await fixture({
    "native.ts": 'import "./ui";',
    "ui.ts":
      "setTimeout(() => {}, 1); navigator.clipboard.readText(); URL.createObjectURL(bytes);",
  });
  await expect(
    checkNativeImportGraph({
      root,
      entry: join(root, "native.ts"),
      uiAdapters: [{ path: "ui.ts", reason: "Focus after the view mounts." }],
    }),
  ).rejects.toThrow(/browser-(clipboard|bytes)/);
});

it("does not confuse injected ports, property names, strings, or type-only imports with browser authority", async () => {
  const root = await fixture({
    "native.ts":
      'import type { BrowserPort } from "./browser"; export function render(fetch: BrowserPort, crypto: () => string) { return { fetch: fetch("message"), crypto: crypto(), text:"localStorage.setItem()" }; }',
    "browser.ts":
      'localStorage.setItem("pending", "write"); export type BrowserPort = (value: string) => string;',
  });
  const report = await checkNativeImportGraph({
    root,
    entry: join(root, "native.ts"),
  });
  expect(report.findings).toEqual([]);
  expect(report.modules).not.toContain("browser.ts");
});

it("resolves imported, destructured, and shadowed native preference bindings", async () => {
  const root = await fixture({
    "native.ts":
      'import { localStorage } from "./port"; localStorage.getItem("view"); export function render({localStorage: storage}) { return storage.getItem("view"); } export function panel(localStorage) { return localStorage.getItem("view"); }',
    "port.ts":
      "export const localStorage = { getItem(name) { return name; } };",
  });
  expect(
    (await checkNativeImportGraph({ root, entry: join(root, "native.ts") }))
      .findings,
  ).toEqual([]);
});

it("does not treat object method names as browser global references", async () => {
  const root = await fixture({
    "native.ts":
      'export const port = { fetch() { return "native"; }, get localStorage() { return "native"; } };',
  });
  expect(
    (await checkNativeImportGraph({ root, entry: join(root, "native.ts") }))
      .findings,
  ).toEqual([]);
});

it("inspects final JavaScript for non-JavaScript source modules", async () => {
  const root = await fixture({
    "native.ts": 'import text from "./contract.json?raw"; console.log(text);',
    "contract.json": '{"version":1}',
  });
  expect(
    (await checkNativeImportGraph({ root, entry: join(root, "native.ts") }))
      .findings,
  ).toEqual([]);
});

it("permits timers only in an explicitly named UI adapter", async () => {
  const root = await fixture({
    "native.ts": 'import "./ui";',
    "ui.ts": "setTimeout(() => document.body.focus(), 1);",
  });
  const options = { root, entry: join(root, "native.ts") };
  await expect(checkNativeImportGraph(options)).rejects.toThrow(
    "business-timer",
  );
  const report = await checkNativeImportGraph({
    ...options,
    uiAdapters: [{ path: "ui.ts", reason: "Focus after the view mounts." }],
  });
  expect(report.findings).toEqual([]);
});

it("does not extend UI permission to imported business code or network calls", async () => {
  const root = await fixture({
    "native.ts": 'import "./ui";',
    "ui.ts": 'import "./business"; setTimeout(() => {}, 1); fetch("/api");',
    "business.ts": "setInterval(() => {}, 1000);",
  });
  const checker = nativeImportGraphPlugin({
    entry: join(root, "native.ts"),
    uiAdapters: [{ path: "ui.ts", reason: "View focus." }],
  });
  await expect(
    build({
      root,
      configFile: false,
      logLevel: "silent",
      plugins: [checker.plugin],
      build: {
        write: false,
        rolldownOptions: { input: join(root, "native.ts"), treeshake: false },
      },
    }),
  ).rejects.toThrow("Native import graph rejected");
  expect(
    checker
      .report()
      .findings.map(({ module, capability }) => [
        module.split("/").at(-1),
        capability,
      ]),
  ).toEqual(
    expect.arrayContaining([
      ["ui.ts", "browser-network"],
      ["business.ts", "business-timer"],
    ]),
  );
});

it("permits only direct read operations in a named migration adapter", async () => {
  const root = await fixture({
    "native.ts": 'import "./migration";',
    "migration.ts":
      'console.log(localStorage.getItem("legacy"), localStorage.key(0), localStorage.length); void indexedDB.databases();',
  });
  const options = {
    root,
    entry: join(root, "native.ts"),
    readonlyMigrations: [
      {
        path: "migration.ts",
        reason: "One-time legacy read before native import.",
      },
    ],
  };
  expect((await checkNativeImportGraph(options)).findings).toEqual([]);
});

it.each([
  'localStorage.setItem("legacy", "changed");',
  'localStorage.removeItem("legacy");',
  "localStorage.length = 0;",
  "const storage = localStorage; storage.clear();",
  'indexedDB.open("new-database");',
  'fetch("/api/import");',
  'crypto.subtle.digest("SHA-256", new Uint8Array());',
])(
  "does not let a migration exception authorize writes or other authority: %s",
  async (source) => {
    const root = await fixture({
      "native.ts": 'import "./migration";',
      "migration.ts": source,
    });
    await expect(
      checkNativeImportGraph({
        root,
        entry: join(root, "native.ts"),
        readonlyMigrations: [
          { path: "migration.ts", reason: "Read legacy settings." },
        ],
      }),
    ).rejects.toThrow("Native import graph rejected");
  },
);

it("requires exact, documented exception paths", async () => {
  const root = await fixture({ "native.ts": "export const view = 1;" });
  await expect(
    checkNativeImportGraph({
      root,
      entry: join(root, "native.ts"),
      uiAdapters: [{ path: "src/**", reason: "all UI" }],
    }),
  ).rejects.toThrow(/exact file path/);
  await expect(
    checkNativeImportGraph({
      root,
      entry: join(root, "native.ts"),
      uiAdapters: [{ path: "native.ts" }],
    }),
  ).rejects.toThrow(/reason/);
});

async function writtenFixture(root, plugins = []) {
  return buildNativeRenderer(
    {
      root,
      configFile: false,
      logLevel: "silent",
      plugins,
      build: {
        outDir: "dist",
        minify: false,
        rolldownOptions: { input: join(root, "native.ts") },
      },
    },
    { entry: join(root, "native.ts") },
  );
}

it("never accepts plugin-only builds without terminal finalization", async () => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  const checker = nativeImportGraphPlugin({ entry: join(root, "native.ts") });
  await build({
    root,
    configFile: false,
    logLevel: "silent",
    plugins: [checker.plugin],
    build: {
      outDir: "dist",
      rolldownOptions: { input: join(root, "native.ts") },
    },
  });
  expect(checker.report().accepted).toBe(false);
  await expect(
    verifyNativeArtifactAudit({ outDir: join(root, "dist") }),
  ).rejects.toThrow();
});
it.each(["late.js", "late.bin"])(
  "rejects an unlisted file from a late write hook: %s",
  async (name) => {
    const root = await fixture({ "native.ts": 'console.log("view");' });
    await expect(
      writtenFixture(root, [
        {
          name: "late-disk-writer",
          async closeBundle() {
            await writeFile(join(root, "dist", name), 'fetch("/api");');
          },
        },
      ]),
    ).rejects.toThrow(/inventory/);
  },
);
it("rejects disk bytes changed after output generation", async () => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  await expect(
    writtenFixture(root, [
      {
        name: "late-disk-rewrite",
        async writeBundle(_options, bundle) {
          const item = Object.values(bundle).find(
            (item) => item.type === "chunk",
          );
          await writeFile(join(root, "dist", item.fileName), 'fetch("/api");');
        },
      },
    ]),
  ).rejects.toThrow(/hash mismatch/);
});
it("requires the complete returned artifact inventory", async () => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  const checker = nativeImportGraphPlugin({ entry: join(root, "native.ts") });
  const output = await build({
    root,
    configFile: false,
    logLevel: "silent",
    plugins: [checker.plugin],
    build: {
      write: false,
      sourcemap: "hidden",
      rolldownOptions: { input: join(root, "native.ts") },
    },
  });
  await expect(
    checker.finalize({
      output: output.output.filter((item) => item.type !== "chunk"),
    }),
  ).rejects.toThrow(/inventory/);
  expect(checker.report().accepted).toBe(false);
});
it("verifies exact written artifacts and refuses stale source or output", async () => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  const built = await writtenFixture(root);
  expect(built.report.accepted).toBe(true);
  expect(
    await verifyNativeArtifactAudit({ outDir: join(root, "dist") }),
  ).toEqual(built.receipt);
  await writeFile(join(root, "native.ts"), 'console.log("changed view");');
  await expect(
    verifyNativeArtifactAudit({ outDir: join(root, "dist") }),
  ).rejects.toThrow(/source|stale/);
});
it("refuses a stale audit after a post-build artifact change", async () => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  const built = await writtenFixture(root);
  await writeFile(
    join(
      root,
      "dist",
      built.receipt.artifacts.find((item) => item.path.endsWith(".js")).path,
    ),
    'fetch("/api");',
  );
  await expect(
    verifyNativeArtifactAudit({ outDir: join(root, "dist") }),
  ).rejects.toThrow(/hash mismatch/);
});
it("refuses a stale audit after an unlisted post-build asset", async () => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  await writtenFixture(root);
  await writeFile(join(root, "dist", "worker.js"), 'fetch("/api");');
  await expect(
    verifyNativeArtifactAudit({ outDir: join(root, "dist") }),
  ).rejects.toThrow(/inventory/);
});
it("refuses a modified audit receipt", async () => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  await writtenFixture(root);
  const path = join(root, "dist", NATIVE_AUDIT_FILENAME);
  const receipt = JSON.parse(await readFile(path, "utf8"));
  receipt.artifacts = [];
  await writeFile(path, JSON.stringify(receipt));
  await expect(
    verifyNativeArtifactAudit({ outDir: join(root, "dist") }),
  ).rejects.toThrow(/stale/);
});
it.each([
  ["worker.js", 'import "https://example.invalid/remote.js";'],
  ["worker.js", 'import("./missing.js");'],
  ["extra.html", "<script src=missing.js></script>"],
  ["extra.html", '<script src="image.png"></script>'],
  ["unknown.wasm", "bytes"],
])("holds unknown executable output %s: %s", async (name, source) => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  await expect(
    emittedFixture(root, [
      {
        name: "unknown-output",
        generateBundle() {
          this.emitFile({ type: "asset", fileName: name, source });
          this.emitFile({
            type: "asset",
            fileName: "image.png",
            source: "fixture",
          });
        },
      },
    ]),
  ).rejects.toThrow(/Uninspected|Unknown/);
});
it("does not let late timer code inherit an approved source owner", async () => {
  const root = await fixture({
    "native.ts": 'import "./ui";',
    "ui.ts": 'requestAnimationFrame(() => console.log("focus"));',
  });
  await expect(
    emittedFixture(
      root,
      [
        {
          name: "late-timer",
          generateBundle: {
            order: "post",
            handler(_options, bundle) {
              for (const item of Object.values(bundle))
                if (item.type === "chunk")
                  item.code += '\nsetTimeout(() => console.log("retry"), 5);';
            },
          },
        },
      ],
      { uiAdapters: [{ path: "ui.ts", reason: "Focus the current view." }] },
    ),
  ).rejects.toThrow(/business-timer/);
});

it("binds artifact acceptance to the caller's exact entry and permission contract", async () => {
  const root = await fixture({ "native.ts": 'console.log("view");' });
  await writtenFixture(root);
  await expect(
    verifyNativeArtifactAudit({
      outDir: join(root, "dist"),
      expected: { root, entry: "native.ts", uiAdapters: [] },
    }),
  ).resolves.toMatchObject({ version: 1 });
  await expect(
    verifyNativeArtifactAudit({
      outDir: join(root, "dist"),
      expected: { root, entry: "other.ts" },
    }),
  ).rejects.toThrow(/contract/);
  await expect(
    verifyNativeArtifactAudit({
      outDir: join(root, "dist"),
      expected: {
        root,
        entry: "native.ts",
        uiAdapters: [{ path: "native.ts", reason: "Different permission." }],
      },
    }),
  ).rejects.toThrow(/contract/);
});

it("rejects known browser owners even when all their code is erased", async () => {
  const root = await fixture({
    "native.ts": 'import "rxdb"; console.log("UI");',
    "node_modules/rxdb/package.json": JSON.stringify({
      name: "rxdb",
      type: "module",
      exports: "./index.js",
      sideEffects: false,
    }),
    "node_modules/rxdb/index.js": 'export const unused = "pure";',
  });
  await expect(emittedFixture(root)).rejects.toThrow(/browser-owner/);
});
it("limits a retained vendor UI exception to timers in the exact named module", async () => {
  const root = await fixture({
    "native.ts": 'import { focus } from "view-kit"; focus();',
    "node_modules/view-kit/package.json": JSON.stringify({
      name: "view-kit",
      type: "module",
      exports: "./index.js",
    }),
    "node_modules/view-kit/index.js":
      'export function focus() { requestAnimationFrame(() => console.log("focus")); }',
  });
  const options = {
    uiAdapters: [
      {
        path: "node_modules/view-kit/index.js",
        reason: "Focus the rendered view.",
      },
    ],
  };
  expect((await emittedFixture(root, [], options)).accepted).toBe(true);
  await writeFile(
    join(root, "node_modules/view-kit/index.js"),
    "export function focus() { requestAnimationFrame(() => navigator.clipboard.readText()); }",
  );
  await expect(emittedFixture(root, [], options)).rejects.toThrow(
    /browser-clipboard/,
  );
});
it("holds an emitted chunk whose source map is missing", async () => {
  const root = await fixture({ "native.ts": 'console.log("UI");' });
  await expect(
    emittedFixture(root, [
      {
        name: "missing-map",
        generateBundle: {
          order: "post",
          handler(_options, bundle) {
            for (const item of Object.values(bundle))
              if (item.type === "chunk") item.map = null;
          },
        },
      },
    ]),
  ).rejects.toThrow(/source map|map asset/);
});

it("holds a source file changed by a late build hook before audit acceptance", async () => {
  const root = await fixture({ "native.ts": 'console.log("UI");' });
  await expect(
    writtenFixture(root, [
      {
        name: "late-source-change",
        async closeBundle() {
          await writeFile(join(root, "native.ts"), 'fetch("/api");');
        },
      },
    ]),
  ).rejects.toThrow(/source.*stale/);
});

it("audits the written HTML, CSS and JavaScript produced by the actual build boundary", async () => {
  const root = await fixture({
    "native.html":
      '<!doctype html><html><head><title>Native UI</title></head><body><div id="root"></div><script type="module" src="./native.ts"></script></body></html>',
    "native.ts": 'import "./theme.css"; console.log("UI");',
    "theme.css": "body { color: red; }",
  });
  const built = await buildNativeRenderer(
    {
      root,
      configFile: false,
      logLevel: "silent",
      build: {
        modulePreload: false,
        outDir: "dist",
        minify: false,
        rolldownOptions: { input: join(root, "native.html") },
      },
    },
    { entry: join(root, "native.html") },
  );
  expect(
    built.receipt.artifacts.some((item) => item.path.endsWith(".html")),
  ).toBe(true);
  expect(
    built.receipt.artifacts.some((item) => item.path.endsWith(".css")),
  ).toBe(true);
  expect(
    built.receipt.artifacts.some((item) => item.path.endsWith(".js")),
  ).toBe(true);
  await expect(
    verifyNativeArtifactAudit({
      outDir: join(root, "dist"),
      expected: { root, entry: "native.html" },
    }),
  ).resolves.toEqual(built.receipt);
});

it("does not accept a caller's different graph entry for an HTML build", async () => {
  const root = await fixture({
    "native.html": '<script type="module" src="./native.ts"></script>',
    "native.ts": 'console.log("UI");',
  });
  await expect(
    buildNativeRenderer(
      {
        root,
        configFile: false,
        logLevel: "silent",
        build: {
          write: false,
          modulePreload: false,
          rolldownOptions: { input: join(root, "native.html") },
        },
      },
      { entry: join(root, "native.ts") },
    ),
  ).rejects.toThrow(/entry|graph/);
});

it.each([
  ["extra.html", '<img srcset="https://example.invalid/image.png 1x">'],
  ["extra.html", "<img srcset=https://example.invalid/image.png>"],
  ["extra.html", '<link imagesrcset="https://example.invalid/image.png 1x">'],
  [
    "extra.html",
    '<div style="background:url(https://example.invalid/x)"></div>',
  ],
  ["extra.html", '<video poster="https://example.invalid/image.png"></video>'],
  [
    "extra.html",
    '<meta http-equiv="refresh" content="0; url=https://example.invalid">',
  ],
  [
    "extra.html",
    "<svg><image style=\"fill:image-set('https://example.invalid/x' 1x)\"></image></svg>",
  ],
  [
    "extra.css",
    'body { background: image-set("https://example.invalid/x" 1x); }',
  ],
  [
    "extra.css",
    'body { background: -webkit-image-set("https://example.invalid/x" 1x); }',
  ],
  ["extra.css", 'body { background: image("https://example.invalid/x"); }'],
  [
    "extra.css",
    'body { background: i\\6d age-set("https://example.invalid/x" 1x); }',
  ],
  [
    "extra.svg",
    '<svg><style>rect { fill:image-set("https://example.invalid/x" 1x); }</style></svg>',
  ],
])(
  "rejects unsupported final asset URL forms in %s: %s",
  async (name, source) => {
    const root = await fixture({ "native.ts": 'console.log("UI");' });
    await expect(
      emittedFixture(root, [
        {
          name: "late-url-form",
          generateBundle() {
            this.emitFile({ type: "asset", fileName: name, source });
          },
        },
      ]),
    ).rejects.toThrow(/Uninspected|Unknown/);
  },
);
it("checks the complete raw source of a retained vendor before function erasure", async () => {
  const root = await fixture({
    "native.ts": 'import { view } from "view-kit"; console.log(view);',
    "node_modules/view-kit/package.json": JSON.stringify({
      name: "view-kit",
      type: "module",
      exports: "./index.js",
    }),
    "node_modules/view-kit/index.js":
      'export function view(value) { return String(value).slice(0, 3); } export function unused() { return fetch("/api"); }',
  });
  await expect(
    emittedFixture(root, [
      {
        name: "erase-vendor-function",
        transform(code, id) {
          if (id.endsWith("view-kit/index.js"))
            return {
              code: "export function view(value) { return String(value).slice(0, 3); }",
              map: null,
            };
        },
      },
    ]),
  ).rejects.toThrow(/browser-network/);
});

it("checks raw project JavaScript before a transform removes an unsafe function", async () => {
  const root = await fixture({
    "native.ts": 'import { view } from "./presentation"; console.log(view);',
    "presentation.ts":
      'export function view(value: string) { return value.slice(0, 3); } export function unused() { return fetch("/api"); }',
  });
  await expect(
    emittedFixture(root, [
      {
        name: "erase-project-function",
        transform(code, id) {
          if (id.endsWith("presentation.ts"))
            return {
              code: "export function view(value) { return value.slice(0, 3); }",
              map: null,
            };
        },
      },
    ]),
  ).rejects.toThrow(/browser-network/);
});
it("hashes an imported binary image as bytes while preserving its UI asset owner", async () => {
  const image = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const root = await fixture({
    "native.ts": 'import image from "./image.png"; console.log(image);',
    "image.png": image,
  });
  const report = await emittedFixture(root);
  expect(report.accepted).toBe(true);
  const owner = report.inventory.modules.find((module) =>
    module.id.endsWith("image.png"),
  );
  expect(owner.sourceHash).toBe(
    createHash("sha256").update(image).digest("hex"),
  );
});

it("keeps raw JSX attributes and type members separate from runtime browser references", async () => {
  const root = await fixture({
    "native.ts": 'import { View } from "./view"; console.log(View);',
    "view.tsx":
      'interface Props { open: boolean; fetch: string; } export function View(props: Props) { return <details open={props.open} data-port="fetch"><input type="file" /></details>; }',
  });
  expect(
    (
      await emittedFixture(root, [
        {
          name: "compile-view",
          transform(code, id) {
            if (id.endsWith("view.tsx"))
              return {
                code: "export function View(props) { return { open: props.open }; }",
                map: null,
              };
          },
        },
      ])
    ).accepted,
  ).toBe(true);
});
it("still rejects an actual browser reference inside a raw JSX expression", async () => {
  const root = await fixture({
    "native.ts": 'import { View } from "./view"; console.log(View);',
    "view.tsx":
      "export function View() { return <details open={Boolean(fetch)} />; }",
  });
  await expect(
    emittedFixture(root, [
      {
        name: "compile-view",
        transform(code, id) {
          if (id.endsWith("view.tsx"))
            return { code: "export function View() { return {}; }", map: null };
        },
      },
    ]),
  ).rejects.toThrow(/browser-network/);
});
