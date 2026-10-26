import type { NativeServerContext } from "../client";
import { createNativeBytes } from "../bytes";
import { createNativePreview, type NativePreviewCleanup } from "../preview";
import type { FileTarget } from "../../fileActions";

/** The server context and native input admission own every privileged action. */
export function createNativeFileServices(
  context: NativeServerContext,
  cleanup: NativePreviewCleanup,
) {
  const local = context.serverId === "local";
  const bytes = createNativeBytes(context);
  const previews = createNativePreview(context, cleanup);
  const requireLocal = () => {
    if (!local)
      throw new Error("This action requires the local server context.");
  };
  const action = (kind: "open" | "reveal" | "preview", target: FileTarget) => {
    requireLocal();
    return context.request<boolean>("desktop.fileAction", {
      value: {
        action: kind,
        target: {
          ...(target.agent === undefined ? {} : { agent: target.agent }),
          ...(target.path === undefined ? {} : { path: target.path }),
          ...(target.asset === undefined ? {} : { asset: target.asset }),
        },
      },
    });
  };
  return {
    previews,
    fileDialogs: {
      canPickDirectory: () => local,
      pickDirectory: async () => {
        requireLocal();
        return context.request<string | null>("desktop.pickDirectory", {});
      },
      canPickFiles: () => true,
      pickFiles: bytes.pickAttachments,
      canRevealPath: () => local,
      revealPath: async (path: string) => {
        requireLocal();
        await context.request("desktop.revealPath", { value: path });
      },
      canShowVaultKeys: () => local,
      showVaultKeys: async (prefix: string) => {
        requireLocal();
        await context.request("desktop.showVaultKeys", { value: prefix });
      },
    },
    fileActions: {
      canUseNativeFiles: () => local,
      openFile: (target: FileTarget) => action("open", target),
      revealFile: (target: FileTarget) => action("reveal", target),
      previewFile: (target: FileTarget) => action("preview", target),
      saveFile: (value: { name: string; data: string; mime?: string }) =>
        context.request<boolean>("desktop.saveText", {
          value: { name: value.name, text: value.data },
        }),
    },
  };
}
