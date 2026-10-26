import type { openNativePreferences } from "../preferenceStore";
import type { NativeServerContext } from "../client";
import type { StudioServer } from "../../servers/registry";
import type { NativeAttachmentHost } from "../attachmentSelection";

export type NativePreferencePort = Storage &
  Pick<
    Awaited<ReturnType<typeof openNativePreferences>>,
    | "pending"
    | "error"
    | "revision"
    | "flush"
    | "subscribe"
    | "acceptSnapshot"
    | "acceptValue"
    | "assertOwner"
  >;
export type NativeServiceReady = Readonly<{
  context: NativeServerContext;
  storage: NativePreferencePort;
}>;
/** Metadata is supplied by the host registry. Only context authorizes commands. */
export type NativeServiceOwner = Readonly<StudioServer>;
export type NativeServiceOptions = Readonly<{
  report(error: unknown): void;
  attachmentHost?: NativeAttachmentHost;
}>;
