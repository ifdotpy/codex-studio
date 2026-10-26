import type { DesktopBridge } from "../../src/desktop.ts";
export function desktopFixture(): DesktopBridge {
  return {
    platform: "fixture",
    requestMicrophone: async () => false,
    prepareTranscription: async () => "",
    transcribeAudio: async () => ({
      text: "",
      provider: "fixture",
      onDevice: false,
    }),
    cancelTranscription: async () => false,
    onTranscriptionProgress: () => () => {},
    pickDirectory: async () => null,
    pickFiles: async () => [],
    revealPath: async () => {},
    openExternal: async () => {},
    showVaultKeys: async () => {},
    onNavigate: () => () => {},
    notify: async () => false,
  };
}
