import { lazy, Suspense } from "react";
import { hasNativeClient } from "../native/client";
import { NativeRecordingPanel } from "../native/NativeRecordingPanel";

const BrowserRecordingPanel = lazy(() =>
  import("./BrowserRecordingPanel").then((module) => ({
    default: module.BrowserRecordingPanel,
  })),
);

export function Dictation(props: {
  chatId: string;
  onInsert: (text: string) => void;
  disabled?: boolean;
}) {
  return hasNativeClient() ? (
    <NativeRecordingPanel {...props} />
  ) : (
    <Suspense fallback={null}>
      <BrowserRecordingPanel {...props} />
    </Suspense>
  );
}
