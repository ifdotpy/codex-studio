import type { DesktopBridge } from "../src/desktop.ts";

function unused(): never {
  throw new Error("Desktop bridge method is outside this fixture");
}

/** Complete bridge fixture: unconfigured capabilities fail visibly if called. */
export function desktopBridgeFixture(
  overrides: Partial<DesktopBridge> = {},
): DesktopBridge {
  return {
    platform: "fixture",
    requestMicrophone: unused,
    prepareTranscription: unused,
    transcribeAudio: unused,
    cancelTranscription: unused,
    onTranscriptionProgress: unused,
    pickDirectory: unused,
    pickFiles: unused,
    revealPath: unused,
    openExternal: unused,
    showVaultKeys: unused,
    onNavigate: unused,
    notify: unused,
    ...overrides,
  };
}
