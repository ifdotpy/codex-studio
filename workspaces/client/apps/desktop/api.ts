import type { components } from "../web/src/generated/api.ts";
import type { StudioServerIdentity } from "../web/src/servers/serverIdentity.ts";
/** HTTP methods supported by the signed server transport. */
export const serverMethods = ["GET", "POST", "HEAD"] as const;
export type ServerMethod = (typeof serverMethods)[number];
export function isServerMethod(value: string): value is ServerMethod {
  return serverMethods.some((method) => method === value);
}
export type PairReceipt = Required<
  Pick<StudioServerIdentity, "id" | "origin" | "label" | "credentialId">
>;
export type AutomaticPairApproval = {
  localServerId: string;
  serverId: string;
  generation: string;
  inviteRequestId: string;
};
export type PairRequest = {
  origin: string;
  invitation: unknown;
  requestId: string;
};
export type ServerRequest = {
  serverId: string;
  credentialId?: string | undefined;
  url: string;
  method: ServerMethod;
  headers?: RequestInit["headers"];
  body?: ArrayBuffer | undefined;
  requestId?: string | undefined;
};
type ResponseMetadata = {
  status: number;
  statusText: string;
  headers: [string, string][];
};
export type BufferedCredentialResponse = ResponseMetadata & {
  body: ArrayBuffer;
  streamId?: never;
};
export type CredentialResponse =
  | BufferedCredentialResponse
  | (ResponseMetadata & { streamId: string; body?: never });
export type StreamPayload =
  | { bytes: ArrayBuffer; done?: never; error?: never }
  | { bytes?: never; done: true; error?: never }
  | { bytes?: never; done?: never; error: string };
export type StreamChunk = {
  serverId: string;
  streamId: string;
} & StreamPayload;
type CredentialActions = {
  pair: { payload: PairRequest; result: PairReceipt };
  pairApproved: {
    payload: PairRequest & { approval: AutomaticPairApproval };
    result: PairReceipt | { notApplied: "invite_unavailable" };
  };
  request: {
    payload: ServerRequest & { streamId: string };
    result: CredentialResponse;
  };
  summary: { payload: ServerRequest; result: BufferedCredentialResponse };
  cancel: {
    payload: {
      serverId: string;
      streamId: string;
      /** Why the frame cancels a stream; shown in the stream log. */
      reason?: string | undefined;
    };
    result: void;
  };
  forget: { payload: { serverId: string }; result: void };
  pairAttempt: {
    payload: {
      origin: string;
      serverId: string;
      requestId: string;
      inviteId?: string | undefined;
    };
    result: boolean;
  };
};
export type CredentialAction = keyof CredentialActions;
const credentialActions = {
  pair: true,
  pairApproved: true,
  request: true,
  summary: true,
  cancel: true,
  forget: true,
  pairAttempt: true,
} satisfies Record<CredentialAction, true>;
export function isCredentialAction(value: string): value is CredentialAction {
  return Object.hasOwn(credentialActions, value);
}
export type CredentialRequest<A extends CredentialAction = CredentialAction> = {
  [K in A]: {
    action: K;
    frameOwner?: string | undefined;
  } & CredentialActions[K]["payload"];
}[A];
export type CredentialResult<A extends CredentialAction = CredentialAction> =
  CredentialActions[A]["result"];
export interface CredentialBridge {
  serverCredentialAction<A extends CredentialAction>(
    request: CredentialRequest<A>,
  ): Promise<CredentialResult<A>>;
  onServerStream(callback: (chunk: StreamChunk) => void): () => void;
}
/** The redirect port of a sign-in that another server approved for this window. */
export type SignInForward = components["schemas"]["BrowserForward"];

/** Native method signatures; IPC requests and results derive from this owner. */
export interface NativeMethods {
  requestMicrophone(): Promise<boolean>;
  prepareTranscription(): Promise<string>;
  transcribeAudio(value: {
    id: string;
    permit: string;
    audio: ArrayBuffer;
    locale: string;
  }): Promise<{ text: string; provider: string; onDevice: boolean }>;
  cancelTranscription(id: string): Promise<boolean>;
  pickDirectory(): Promise<string | null>;
  pickFiles(): Promise<
    { name: string; path: string; mime: string; data: string }[]
  >;
  revealPath(path: string): Promise<void>;
  saveFile(value: { name: string; data: ArrayBuffer }): Promise<boolean>;
  fileAction(value: {
    action: "open" | "reveal" | "preview";
    target: { agent?: string; path?: string; asset?: string };
  }): Promise<boolean>;
  openExternal(url: string): Promise<void>;
  /** Open an approved sign-in link and bring the browser to the front. With `forward`, this
   * Mac first holds the redirect port for the command on the server of the frame. */
  openSignIn(
    value: string | { url: string; forward: SignInForward },
  ): Promise<void>;
  /** Show the paper key of a new vault in Terminal (layr-desk keys). */
  showVaultKeys(prefix: string): Promise<void>;
  getBackendUpdate(): Promise<{
    availableBackendBuild: string | null;
    updateRequired: boolean | null;
  }>;
  notify(value: {
    title: string;
    body: string;
    /** Play the system sound (sign-in cards); other alerts are silent. */
    sound?: boolean;
    target: {
      serverId?: string;
      agentId: string;
      section: "messages";
      itemId?: string;
    };
  }): Promise<boolean>;
}
type OptionalNativeMethod =
  | "saveFile"
  | "fileAction"
  | "openSignIn"
  | "getBackendUpdate";
/** Methods allowed through the server wrapper; direct local frames also allow showVaultKeys. */
export const serverNativeMethods = {
  requestMicrophone: true,
  prepareTranscription: true,
  transcribeAudio: true,
  cancelTranscription: true,
  pickDirectory: true,
  pickFiles: true,
  revealPath: true,
  saveFile: true,
  fileAction: true,
  openExternal: true,
  openSignIn: true,
  getBackendUpdate: true,
} satisfies Partial<Record<keyof NativeMethods, true>>;
export type ServerNativeMethod = keyof typeof serverNativeMethods;
export function isServerNativeMethod(
  value: string,
): value is ServerNativeMethod {
  return Object.hasOwn(serverNativeMethods, value);
}
type MethodRequest<K extends keyof NativeMethods> = {
  method: K;
} & (Parameters<NativeMethods[K]> extends [infer V]
  ? { value: V }
  : { value?: undefined });
export type ServerNativeRequest<
  M extends ServerNativeMethod = ServerNativeMethod,
> = {
  [K in M]: MethodRequest<K>;
}[M];
export type NativeMethod =
  | keyof NativeMethods
  | "serverCredentialAction"
  | "serverNativeAction"
  | "releaseServerView";
export type NativeRequest<M extends NativeMethod = NativeMethod> = {
  [K in M]: K extends keyof NativeMethods
    ? MethodRequest<K>
    : K extends "serverCredentialAction"
      ? { method: K; value: CredentialRequest }
      : K extends "serverNativeAction"
        ? { method: K; value: ServerNativeRequest & { serverId: string } }
        : { method: K; value: string };
}[M];
export type NativeResult<M extends NativeMethod = NativeMethod> =
  M extends keyof NativeMethods
    ? Awaited<ReturnType<NativeMethods[M]>>
    : M extends "serverCredentialAction"
      ? CredentialResult
      : M extends "serverNativeAction"
        ? NativeResult<ServerNativeMethod>
        : void;
export type NativeCall<M extends NativeMethod = NativeMethod> = {
  [K in M]: NativeRequest<K> extends { value: infer V }
    ? [method: K, value: V]
    : [method: K, value?: undefined];
}[M];
export interface NativeInvoke {
  <A extends CredentialAction>(
    args: [method: "serverCredentialAction", value: CredentialRequest<A>],
    needsGesture?: boolean,
  ): Promise<CredentialResult<A>>;
  <M extends NativeMethod>(
    args: NativeCall<M>,
    needsGesture?: boolean,
    serverId?: string,
  ): Promise<NativeResult<M>>;
}
export interface DesktopBridge
  extends
    Pick<NativeMethods, Exclude<keyof NativeMethods, OptionalNativeMethod>>,
    Partial<Pick<NativeMethods, OptionalNativeMethod>>,
    Partial<CredentialBridge> {
  serverViewOrigin?: string | undefined;
  serverNativeAction?<M extends ServerNativeMethod>(
    serverId: string,
    ...args: NativeCall<M>
  ): Promise<NativeResult<M>>;
  platform: string;
  /** The desktop can hold the redirect port of a sign-in on another server. */
  signInForward?: boolean;
  onTranscriptionProgress(
    callback: (value: { id: string; completed: number; total: number }) => void,
  ): () => void;
  onNavigate(
    callback: (
      target: Parameters<NativeMethods["notify"]>[0]["target"],
    ) => void,
  ): () => void;
}
declare global {
  interface Window {
    codexDesktop?: DesktopBridge;
  }
}
