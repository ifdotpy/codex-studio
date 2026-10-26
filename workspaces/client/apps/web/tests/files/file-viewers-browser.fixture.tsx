import { desktopFixture } from "../files/desktop.fixture.ts";
import type { DesktopBridge } from "../../src/desktop.ts";
declare global {
  interface Window {
    fileViewersBrowserParseDelimited: typeof parseDelimited;
    fileViewersBrowserSafeSvg: typeof safeSvg;
    fileViewersBrowserActions: Parameters<
      NonNullable<DesktopBridge["fileAction"]>
    >[0][];
    fileViewersBrowserSaved: { name: string; bytes: number[] };
    fileViewersBrowserShow: (path: string, line?: number) => void;
    fileViewersBrowserSvgExecuted?: boolean;
  }
}
import { required } from "../teams/helpers.ts";
import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import FilePreview from "../../src/components/FilePreview.tsx";
import StreamingText from "../../src/components/conversation/transcript/StreamingText.tsx";
import {
  parseDelimited,
  safeSvg,
} from "../../src/components/filePreviewFormats.ts";
window.fileViewersBrowserParseDelimited = parseDelimited;
window.fileViewersBrowserSafeSvg = safeSvg;
window.fileViewersBrowserActions = [];
window.codexDesktop = {
  ...desktopFixture(),
  fileAction: async (value) => {
    window.fileViewersBrowserActions.push(value);
    return true;
  },
  saveFile: async (value) => {
    window.fileViewersBrowserSaved = {
      name: value.name,
      bytes: Array.from(new Uint8Array(value.data)),
    };
    return true;
  },
};
function App() {
  const [target, setTarget] =
    useState<React.ComponentProps<typeof FilePreview>["target"]>(null);
  window.fileViewersBrowserShow = (path, line) =>
    setTarget({ agent: "agent-a", path, line });
  return (
    <MantineProvider>
      <StreamingText
        agentId="agent-a"
        text="[Open document](/docs/readme.md)"
      />
      <FilePreview target={target} onClose={() => setTarget(null)} />
    </MantineProvider>
  );
}
createRoot(required(document.getElementById("root"))).render(<App />);
