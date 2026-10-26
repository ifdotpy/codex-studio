import { dirname, extname, isAbsolute, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../src/", import.meta.url));
// These are exact maintained modules, not directory or capability exemptions.
const modules = [
  ["components/FilePreview.tsx", "native/FilePreview.tsx"],
  ["components/RichPreview.tsx", "native/RichPreview.tsx"],
  ["components/PreviewModal.tsx", "native/PreviewModal.tsx"],
  ["components/CodeBlock.tsx", "native/CodeBlock.tsx"],
  ["components/MarkdownImage.tsx", "native/MarkdownImage.tsx"],
  ["native/NativeMarkdown.tsx", "native/PreparedMarkdown.tsx"],
  ["servers/registry.ts", "native/registry.ts"],
  ["servers/serverAliases.ts", "native/serverAliases.ts"],
  ["uuid.ts", "native/identities.ts"],
  ["hooks.ts", "native/hooks.ts"],
  ["servers/ServerServicesContext.tsx", "native/servicesContext.tsx"],
  ["downloads.ts", "native/downloads.ts"],
  ["components/Dictation.tsx", "native/NativeRecordingPanel.tsx"],
  ["components/RealtimeVoice.tsx", "native/NativeRealtimeVoice.tsx"],
  ["components/sidebar/services.tsx", "native/sidebarServices.tsx"],
  ["components/Sidebar.tsx", "native/Sidebar.tsx"],
  ["components/useSidebarOrder.ts", "native/useSidebarOrder.ts"],
  ["components/useSkillAutocomplete.ts", "native/useSkillAutocomplete.ts"],
  [
    "components/useSendingMessageRemoval.ts",
    "native/useSendingMessageRemoval.ts",
  ],
  ["components/removedMessages.ts", "native/removedMessages.ts"],
];

export const nativePlatformAliases = modules.map(([source, target]) => {
  const original = resolve(root, source);
  const extension = extname(original);
  const stem = original.slice(0, -extension.length);
  const name = stem.slice(stem.lastIndexOf("/") + 1);
  return {
    find: new RegExp(`(?:^|/)${name}(?:\\.[cm]?[jt]sx?)?(?:\\?.*)?$`),
    replacement: "$&",
    async customResolver(importee, importer, options) {
      const query = importee.indexOf("?");
      const path = query < 0 ? importee : importee.slice(0, query);
      if (isAbsolute(path) || (importer && path.startsWith("."))) {
        const resolved = resolve(
          dirname(importer?.split("?")[0] ?? original),
          path,
        );
        if (resolved === original || resolved === stem)
          return (
            resolve(root, target) + (query < 0 ? "" : importee.slice(query))
          );
      }
      // An identically named module in another folder keeps its normal owner.
      return this.resolve(importee, importer, { ...options, skipSelf: true });
    },
  };
});
