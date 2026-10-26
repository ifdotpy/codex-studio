import { lazy, Suspense } from "react";
import { hasNativeClient } from "../native/client";
import NativeRealtimeVoice from "../native/NativeRealtimeVoice";
const BrowserRealtimeVoice = lazy(() => import("./BrowserRealtimeVoice"));

export default function RealtimeVoice(props: {
  agentId: string;
  notify: (text: string) => void;
}) {
  return hasNativeClient() ? (
    <NativeRealtimeVoice {...props} />
  ) : (
    <Suspense fallback={null}>
      <BrowserRealtimeVoice {...props} />
    </Suspense>
  );
}
