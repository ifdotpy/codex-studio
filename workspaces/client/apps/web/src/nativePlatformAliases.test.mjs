import { afterEach, expect, it } from "vitest";
import { createServer } from "vite";
import { fileURLToPath } from "node:url";
import { nativePlatformAliases } from "../scripts/nativePlatformAliases.mjs";

const root = fileURLToPath(new URL("../", import.meta.url));
const servers = [];
afterEach(async () => {
  await Promise.all(servers.splice(0).map((server) => server.close()));
});

async function resolver(native) {
  const server = await createServer({
    root,
    configFile: false,
    logLevel: "silent",
    resolve: { alias: native ? nativePlatformAliases : [] },
    server: { middlewareMode: true, watch: null, hmr: false },
    optimizeDeps: { noDiscovery: true, include: [] },
  });
  servers.push(server);
  return (source, importer) =>
    server.environments.client.pluginContainer.resolveId(
      source,
      fileURLToPath(new URL(importer, import.meta.url)),
    );
}

it("resolves exact real App owners to their maintained native modules", async () => {
  const resolve = await resolver(true);
  for (const [source, importer, target] of [
    [
      "../../FilePreview",
      "./components/conversation/transcript/ConversationResults.tsx",
      "./native/FilePreview.tsx",
    ],
    [
      "../../RichPreview",
      "./components/conversation/transcript/ConversationResults.tsx",
      "./native/RichPreview.tsx",
    ],
    [
      "./PreviewModal",
      "./components/FilePreviewView.tsx",
      "./native/PreviewModal.tsx",
    ],
    ["./CodeBlock", "./components/MarkdownView.tsx", "./native/CodeBlock.tsx"],
    [
      "./MarkdownImage",
      "./components/MarkdownView.tsx",
      "./native/MarkdownImage.tsx",
    ],
    [
      "../../../native/NativeMarkdown",
      "./components/conversation/transcript/StreamingText.tsx",
      "./native/PreparedMarkdown.tsx",
    ],
    ["./servers/registry", "./App.tsx", "./native/registry.ts"],
    [
      "../servers/serverAliases",
      "./components/Sidebar.tsx",
      "./native/serverAliases.ts",
    ],
    ["./uuid", "./App.tsx", "./native/identities.ts"],
    ["./hooks", "./App.tsx", "./native/hooks.ts"],
    [
      "./servers/ServerServicesContext",
      "./App.tsx",
      "./native/servicesContext.tsx",
    ],
    [
      "../servers/ServerServicesContext",
      "./components/Composer.tsx",
      "./native/servicesContext.tsx",
    ],
    ["../downloads", "./components/FilePreview.tsx", "./native/downloads.ts"],
    [
      "./components/Dictation",
      "./App.tsx",
      "./native/NativeRecordingPanel.tsx",
    ],
    [
      "./components/RealtimeVoice",
      "./App.tsx",
      "./native/NativeRealtimeVoice.tsx",
    ],
    [
      "./components/sidebar/services",
      "./App.tsx",
      "./native/sidebarServices.tsx",
    ],
    [
      "../components/Sidebar",
      "./servers/CombinedServerSidebar.tsx",
      "./native/Sidebar.tsx",
    ],
    [
      "./useSidebarOrder",
      "./components/SidebarView.tsx",
      "./native/useSidebarOrder.ts",
    ],
    [
      "../useSkillAutocomplete",
      "./components/prompt-composer/PromptInput.tsx",
      "./native/useSkillAutocomplete.ts",
    ],
    [
      "./components/useSendingMessageRemoval",
      "./App.tsx",
      "./native/useSendingMessageRemoval.ts",
    ],
    [
      "./removedMessages",
      "./components/Conversation.tsx",
      "./native/removedMessages.ts",
    ],
  ])
    expect((await resolve(source, importer))?.id).toBe(
      fileURLToPath(new URL(target, import.meta.url)),
    );
});

it("keeps native imports, pure modules and unmatched same-name files unchanged", async () => {
  const resolve = await resolver(true);
  for (const [source, importer, target] of [
    ["./servicesContext", "./native/hooks.ts", "./native/servicesContext.tsx"],
    [
      "./snapshotSelection",
      "./hooks/presentation.ts",
      "./hooks/snapshotSelection.ts",
    ],
    ["./api", "./native/serverServices.ts", "./native/api.ts"],
    ["./hooks", "./native/serverServices.ts", "./native/hooks.ts"],
    ["./downloads", "./native/serverServices.ts", "./native/downloads.ts"],
    ["../apiErrors", "./native/api.ts", "./apiErrors.ts"],
    ["./api", "./App.tsx", "./api.ts"],
    [
      "./serverServices",
      "./servers/MultiServerApp.tsx",
      "./servers/serverServices.ts",
    ],
    ["./sync/uiPreferenceStore", "./App.tsx", "./sync/uiPreferenceStore.ts"],
  ])
    expect((await resolve(source, importer))?.id).toBe(
      fileURLToPath(new URL(target, import.meta.url)),
    );
});

it("leaves the original browser owners unchanged without the native config", async () => {
  const resolve = await resolver(false);
  for (const source of [
    "./uuid",
    "./hooks",
    "./servers/ServerServicesContext",
    "./downloads",
    "./components/Dictation",
    "./components/RealtimeVoice",
    "./components/sidebar/services",
    "./components/Sidebar",
    "./components/useSidebarOrder",
    "./components/useSkillAutocomplete",
    "./components/useSendingMessageRemoval",
    "./components/removedMessages",
  ]) {
    const resolved = await resolve(source, "./App.tsx");
    expect(resolved?.id).not.toContain("/src/native/");
    expect(resolved?.id).toMatch(/\.(?:ts|tsx)$/);
  }
});
